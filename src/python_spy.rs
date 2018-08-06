use std;
use std::mem::size_of;
use std::slice;
use std::path::Path;

use failure::{Error, ResultExt};
use read_process_memory::{Pid, TryIntoProcessHandle, copy_address, ProcessHandle};
use proc_maps::{get_process_maps, MapRange, maps_contain_addr, pid_t};
use python_bindings::{v2_7_15, v3_3_7, v3_5_5, v3_6_6, v3_7_0};

use python_interpreters;
use stack_trace::{StackTrace, get_stack_traces};
use binary_parser::{parse_binary, BinaryInfo};
use utils::{copy_struct, copy_pointer};
use python_interpreters::{InterpreterState, ThreadState};

#[derive(Debug)]
pub struct PythonSpy {
    pub pid: u32,
    pub process: ProcessHandle,
    pub version: Version,
    pub interpreter_address: usize,
    pub threadstate_address: usize,
    pub python_filename: String,
    pub python_install_path: String,
    pub version_string: String
}

impl PythonSpy {
    pub fn new(pid: u32) -> Result<PythonSpy, Error> {
        let process = (pid as Pid).try_into_process_handle().context("Failed to open target process")?;

        // get basic process information (memory maps/symbols etc)
        let python_info = PythonProcessInfo::new(pid)?;

        let version = get_python_version(&python_info, process)?;
        let interpreter_address = get_interpreter_address(&python_info, process, &version)?;

        // lets us figure out which thread has the GIL
        let threadstate_address = match python_info.get_symbol("_PyThreadState_Current") {
            Some(&addr) => addr as usize,
            None => 0
        };

        // Figure out the base path of the python install
        let python_install_path = {
            let mut python_path = Path::new(&python_info.python_filename);
            if let Some(parent) = python_path.parent() {
                python_path = parent;
                if python_path.to_str().unwrap().ends_with("/bin") {
                    if let Some(parent) = python_path.parent() {
                        python_path = parent;
                    }
                }
            }
            python_path.to_str().unwrap().to_string()
        };

        let version_string = format!("python{}.{}", version.major, version.minor);

        Ok(PythonSpy{pid, process, version, interpreter_address, threadstate_address,
                     python_filename: python_info.python_filename,
                     python_install_path,
                     version_string})
    }

    /// Creates a PythonSpy object, retrying up to max_retries times
    /// mainly useful for the case where the process is just started and
    /// symbols/python interpreter might not be loaded yet
    pub fn retry_new(pid: u32, max_retries:u64) -> Result<PythonSpy, Error> {
        let mut retries = 0;
        loop {
            let err = match PythonSpy::new(pid) {
                Ok(process) => {
                    // verify that we can load a stack trace before returning success
                    match process.get_stack_traces() {
                        Ok(_) => return Ok(process),
                        Err(err) => err
                    }
                },
                Err(err) => err
            };

            // If we failed, retry a couple times before returning the last error
            retries += 1;
            if retries >= max_retries {
                return Err(err);
            }
            // TODO: logging
            // println!("Failed to connect to process, retrying. Error: {}", err);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Gets a StackTrace for each thread in the current process
    pub fn get_stack_traces(&self) -> Result<Vec<StackTrace>, Error> {
        match self.version {
            // Currently 3.7.x and 3.8.0a0 have the same ABI, but this might change
            // as 3.8 evolvess
            Version{major: 3, minor: 8, ..} => self._get_stack_traces::<v3_7_0::_is>(),
            Version{major: 3, minor: 7, ..} => self._get_stack_traces::<v3_7_0::_is>(),
            Version{major: 3, minor: 6, ..} => self._get_stack_traces::<v3_6_6::_is>(),
            // ABI for 3.4 and 3.5 is the same for our purposes
            Version{major: 3, minor: 5, ..} => self._get_stack_traces::<v3_5_5::_is>(),
            Version{major: 3, minor: 4, ..} => self._get_stack_traces::<v3_5_5::_is>(),
            Version{major: 3, minor: 3, ..} => self._get_stack_traces::<v3_3_7::_is>(),
            // ABI for 2.3/2.4/2.5/2.6/2.7 is also compatible
            Version{major: 2, minor: 3...7, ..} => self._get_stack_traces::<v2_7_15::_is>(),
            _ => Err(format_err!("Unsupported version of Python: {}", self.version)),
        }
    }

    // implementation of get_stack_traces, where we have a type for the InterpreterState
    fn _get_stack_traces<I: InterpreterState>(&self) -> Result<Vec<StackTrace>, Error> {
        // figure out what thread has the GIL by inspecting _PyThreadState_Current
        let mut gil_thread_id = 0;
        if self.threadstate_address > 0 {
            let addr: usize = copy_struct(self.threadstate_address, &self.process)?;
            if addr != 0 {
                let threadstate: I::ThreadState = copy_struct(addr, &self.process)?;
                gil_thread_id = threadstate.thread_id();
            }
        }

        // Get the stack traces for each thread
        let interp: I = copy_struct(self.interpreter_address, &self.process)
            .context("Failed to copy PyInterpreterState from process")?;
        let mut traces = get_stack_traces(&interp, &self.process)?;

        // annotate traces to indicate which thread is holding the gil (if any),
        // and to provide a shortened filename
        for trace in &mut traces {
            if trace.thread_id == gil_thread_id {
                trace.owns_gil = true;
            }
            for frame in &mut trace.frames {
                frame.short_filename = Some(self.shorten_filename(&frame.filename).to_owned());
            }
        }
        Ok(traces)
    }

    /// We want to display filenames without the boilerplate of the python installation
    /// directory etc. This strips off common prefixes from python library code.
    pub fn shorten_filename<'a>(&self, filename: &'a str) -> &'a str {
        if filename.starts_with(&self.python_install_path) {
            let mut filename = &filename[self.python_install_path.len() + 1..];
            if filename.starts_with("lib") {
                filename = &filename[4..];
                if filename.starts_with(&self.version_string) {
                    filename = &filename[self.version_string.len() + 1..];
                }
                if filename.starts_with("site-packages") {
                    filename = &filename[14..];
                }
            }
            filename
        } else {
            filename
        }
    }
}
/// Returns the version of python running in the process.
fn get_python_version(python_info: &PythonProcessInfo, process: ProcessHandle)
        -> Result<Version, Error> {
    // If possible, grab the sys.version string from the processes memory (mac osx).
    if let Some(&addr) = python_info.get_symbol("Py_GetVersion.version") {
        return Ok(Version::scan_bytes(&copy_address(addr as usize, 128, &process)?)?);
    }

    // otherwise get version info from scanning BSS section for sys.version string
    let bss = copy_address(python_info.python_binary.bss_addr as usize,
                           python_info.python_binary.bss_size as usize, &process)?;
    match Version::scan_bytes(&bss) {
        Ok(version) => Ok(version),
        Err(err) => {
            match python_info.libpython_binary {
                // Before giving up, try again if there is a libpython.so
                Some(ref libpython) => {
                    let bss = copy_address(libpython.bss_addr as usize,
                                           libpython.bss_size as usize, &process)?;
                    Ok(Version::scan_bytes(&bss)?)
                },
                None => Err(err)
            }
        }
    }
}

fn get_interpreter_address(python_info: &PythonProcessInfo,
                           process: ProcessHandle,
                           version: &Version) -> Result<usize, Error> {
    // get the address of the main PyInterpreterState object from loaded symbols if we can
    // (this tends to be faster than scanning through the bss section)
    match version {
        Version{major: 3, minor: 7, ..} => {
            if let Some(&addr) = python_info.get_symbol("_PyRuntime") {
                // TODO: we actually want _PyRuntime.interpeters.head, and probably should
                // generate bindings for the pyruntime object rather than hardcode the offset (24) here
                return Ok(copy_struct((addr + 24) as usize, &process)?);
            }
        },
        _ => {
            if let Some(&addr) = python_info.get_symbol("interp_head") {
                return Ok(copy_struct(addr as usize, &process)
                    .context("Failed to copy PyInterpreterState location from process")?);
            }
        }
    };

    // try scanning the BSS section of the binary for things that might be the interpreterstate
    match get_interpreter_address_from_binary(&python_info.python_binary, &python_info.maps, process, version) {
        Ok(addr) => Ok(addr),
        // Before giving up, try again if there is a libpython.so
        Err(err) => {
            match python_info.libpython_binary {
                Some(ref libpython) => {
                    Ok(get_interpreter_address_from_binary(libpython, &python_info.maps, process, version)?)
                },
                None => Err(err)
            }
        }
    }
}

fn get_interpreter_address_from_binary(binary: &BinaryInfo,
                                       maps: &[MapRange],
                                       process: ProcessHandle,
                                       version: &Version) -> Result<usize, Error> {
    // different versions have different layouts, check as appropiate
    match version {
        Version{major: 3, minor: 8, ..} => check_addresses::<v3_7_0::_is>(binary, maps, process),
        Version{major: 3, minor: 7, ..} => check_addresses::<v3_7_0::_is>(binary, maps, process),
        Version{major: 3, minor: 6, ..} => check_addresses::<v3_6_6::_is>(binary, maps, process),
        Version{major: 3, minor: 5, ..} => check_addresses::<v3_5_5::_is>(binary, maps, process),
        Version{major: 3, minor: 4, ..} => check_addresses::<v3_5_5::_is>(binary, maps, process),
        Version{major: 3, minor: 3, ..} => check_addresses::<v3_3_7::_is>(binary, maps, process),
        Version{major: 2, minor: 3...7, ..} => check_addresses::<v2_7_15::_is>(binary, maps, process),
        _ => Err(format_err!("Unsupported version of Python: {}", version))
    }
}

// Checks whether a block of memory (from BSS/.data etc) contains pointers that are pointing
// to a valid PyInterpreterState
fn check_addresses<I>(binary: &BinaryInfo,
                      maps: &[MapRange],
                      process: ProcessHandle) -> Result<usize, Error>
        where I: python_interpreters::InterpreterState {
    // We're going to scan the BSS/data section for things, and try to narrowly scan things that
    // look like pointers to PyinterpreterState
    let bss = copy_address(binary.bss_addr as usize, binary.bss_size as usize, &process)?;

    #[cfg_attr(feature = "cargo-clippy", allow(cast_ptr_alignment))]
    let addrs = unsafe { slice::from_raw_parts(bss.as_ptr() as *const usize, bss.len() / size_of::<usize>()) };

    for &addr in addrs {
        // TODO: this doesn't seem to work on windows (pointer addresses outside of map ranges)
        if maps_contain_addr(addr, maps) {
            // this address points to valid memory. try loading it up as a PyInterpreterState
            // to further check
            let interp: I = copy_struct(addr, &process)?;

            // get the pythreadstate pointer from the interpreter object, and if it is also
            // a valid pointer then load it up.
            let threads = interp.head();
            if maps_contain_addr(threads as usize, maps) {
                // If the threadstate points back to the interpreter like we expect, then
                // this is almost certainly the address of the intrepreter
                let thread = copy_pointer(threads, &process)?;

                // as a final sanity check, try getting the stack_traces, and only return if this works
                if thread.interp() as usize == addr && get_stack_traces(&interp, &process).is_ok() {
                    return Ok(addr);
                }
            }
        }
    }
    Err(format_err!("Failed to find a python interpreter in the .data section"))
}

/// Holds information about the python process: memory map layout, parsed binary info
/// for python /libpython etc.
pub struct PythonProcessInfo {
    python_binary: BinaryInfo,
    // if python was compiled with './configure --enabled-shared', code/symbols will
    // be in a libpython.so file instead of the executable. support that.
    libpython_binary: Option<BinaryInfo>,
    maps: Vec<MapRange>,
    python_filename: String,
}

impl PythonProcessInfo {
    fn new(pid: u32) -> Result<PythonProcessInfo, Error> {
        // get virtual memory layout
        let maps = get_process_maps(pid as pid_t)?;

        // parse the main python binary
        let (python_binary, python_filename) = {
            #[cfg(unix)]
            let python_bin_pattern = "bin/python";

            #[cfg(windows)]
            let python_bin_pattern = "python.exe";

            let map = maps.iter()
                .find(|m| if let Some(pathname) = &m.filename() {
                    pathname.contains(python_bin_pattern) && m.is_exec()
                } else {
                    false
                }).ok_or_else(|| format_err!("Couldn't find python binary"))?;

            let filename = map.filename().clone().unwrap();
            // TODO: consistent types? u64 -> usize? for map.start etc
            let mut python_binary = parse_binary(&filename, map.start() as u64)?;

            // windows symbols are stored in separate files (.pdb), load
            #[cfg(windows)]
            python_binary.symbols.extend(get_windows_python_symbols(pid, &filename, map.start() as u64)?);

            // For OSX, need to adjust main binary symbols by substracting _mh_execute_header
            // (which we've added to by map.start already, so undo that here)
            #[cfg(target_os = "macos")]
            {
                let offset = python_binary.symbols["_mh_execute_header"] - map.start() as u64;
                for address in python_binary.symbols.values_mut() {
                    *address -= offset;
                }

                if python_binary.bss_addr != 0 {
                    python_binary.bss_addr -= offset;
                }
            }
            (python_binary, filename)
        };

        // likewise handle libpython for python versions compiled with --enabled-shared
        let libpython_binary = {
            #[cfg(unix)]
            let is_python_lib = |pathname: &str| pathname.contains("lib/libpython");

            #[cfg(windows)]
            let is_python_lib = |pathname: &str| pathname.contains("\\python") && pathname.ends_with("dll");

            let libmap = maps.iter()
                .find(|m| if let Some(ref pathname) = &m.filename() {
                    is_python_lib(pathname) && m.is_exec()
                } else {
                    false
                });

            let mut libpython_binary: Option<BinaryInfo> = None;
            if let Some(libpython) = libmap {
                if let Some(filename) = &libpython.filename() {
                    let mut parsed = parse_binary(filename, libpython.start() as u64)?;
                    #[cfg(windows)]
                    parsed.symbols.extend(get_windows_python_symbols(pid, filename, libpython.start() as u64)?);
                    libpython_binary = Some(parsed);
                }
            }
            libpython_binary
        };

        Ok(PythonProcessInfo{python_binary, libpython_binary, maps, python_filename})
    }

    pub fn get_symbol(&self, symbol: &str) -> Option<&u64> {
        if let Some(addr) = self.python_binary.symbols.get(symbol) {
            return Some(addr);
        }

        match self.libpython_binary {
            Some(ref binary) => binary.symbols.get(symbol),
            None => None
        }
    }
}

// We can't use goblin to parse external symbol files (like in a separate .pdb file) on windows,
// So use the win32 api to load up the couple of symbols we need on windows. Note:
// we still can get export's from the PE file
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
pub fn get_windows_python_symbols(pid: u32, filename: &str, base_addr: u64) -> std::io::Result<HashMap<String, u64>> {
    use proc_maps::win_maps::SymbolLoader;

    let handler = SymbolLoader::new(pid as u64)?;
    let _module = handler.load_module(filename)?; // need to keep this module in scope

    let mut ret = HashMap::new();

    // currently we only need a subset of symbols, and enumerating the symbols is
    // expensive (via SymEnumSymbolsW), so rather than load up all symbols like we
    // do for goblin, just load the the couple we need directly.
    for symbol in ["_PyThreadState_Current", "interp_head", "_PyRuntime"].iter() {
        if let Ok((base, addr)) = handler.address_from_name(symbol) {
            ret.insert(String::from(*symbol), base_addr + addr  - base as u64);
        }
    }

    Ok(ret)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub release_flags: String
}

impl Version {
    pub fn scan_bytes(data: &[u8]) -> Result<Version, Error> {
        use regex::bytes::Regex;
        let re = Regex::new(r"(?:\D|^)((\d)\.(\d)\.(\d{1,2}))((a|b|c|rc)\d{1,2})? (.{1,64})").unwrap();

        if let Some(cap) = re.captures_iter(data).next() {
            let release = match cap.get(5) {
                Some(x) => { std::str::from_utf8(x.as_bytes())? },
                None => ""
            };
            let major = std::str::from_utf8(&cap[2])?.parse::<u64>()?;
            let minor = std::str::from_utf8(&cap[3])?.parse::<u64>()?;
            let patch = std::str::from_utf8(&cap[4])?.parse::<u64>()?;
            return Ok(Version{major, minor, patch, release_flags:release.to_owned()});
        }
        Err(format_err!("failed to find version string"))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}.{}{}", self.major, self.minor, self.patch, self.release_flags)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_version() {
        let version = Version::scan_bytes(b"2.7.10 (default, Oct  6 2017, 22:29:07)").unwrap();
        assert_eq!(version, Version{major: 2, minor: 7, patch: 10, release_flags: "".to_owned()});

        let version = Version::scan_bytes(b"3.6.3 |Anaconda custom (64-bit)| (default, Oct  6 2017, 12:04:38)").unwrap();
        assert_eq!(version, Version{major: 3, minor: 6, patch: 3, release_flags: "".to_owned()});

        let version = Version::scan_bytes(b"Python 3.7.0rc1 (v3.7.0rc1:dfad352267, Jul 20 2018, 13:27:54)").unwrap();
        assert_eq!(version, Version{major: 3, minor: 7, patch: 0, release_flags: "rc1".to_owned()});

        let version = Version::scan_bytes(b"53.7.0rc1 (v53.7.0rc1:dfad352267, Jul 20 2018, 13:27:54)");
        assert!(version.is_err(), "Shouldn't allow v53 of python (yet)");

        let version = Version::scan_bytes(b"3.7 10 ");
        assert!(version.is_err(), "needs dotted version");

        let version = Version::scan_bytes(b"3.7.10fooboo ");
        assert!(version.is_err(), "limit suffixes");
    }
}