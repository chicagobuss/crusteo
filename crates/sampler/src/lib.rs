//! A sampling profiler for one thread of this process.
//!
//! A background thread suspends the target about once a millisecond, reads
//! its registers, walks its stack with the x64 unwind tables, and resumes it.
//! When the window closes, return addresses are symbolized with dbghelp,
//! inlined frames included, and written as folded stacks, one line per
//! distinct stack: `root;caller;...;leaf count`. Flamegraph tools read the
//! format directly.
//!
//! It needs no admin rights, unlike ETW: a process may always inspect its own
//! threads. Samples are wall-clock, so time the thread spends waiting shows up
//! too, as stacks ending in a wait such as `NtUserGetMessage`.
//!
//! Enable with `CRUSTEO_PROFILE=<out file>`. `CRUSTEO_PROFILE_DELAY` is the
//! seconds to wait before sampling (default 10) and `CRUSTEO_PROFILE_SECS`
//! how long to sample (default 10).

use std::path::PathBuf;
use std::time::Duration;

/// Starts sampling the calling thread if `CRUSTEO_PROFILE` is set. Call it
/// from the thread to profile, before it enters its event loop.
pub fn from_env() {
    let Some(path) = std::env::var_os("CRUSTEO_PROFILE") else {
        return;
    };
    let secs = |name: &str, default: f64| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(default)
    };
    let delay = Duration::from_secs_f64(secs("CRUSTEO_PROFILE_DELAY", 10.));
    let length = Duration::from_secs_f64(secs("CRUSTEO_PROFILE_SECS", 10.));
    imp::start(PathBuf::from(path), delay, length);
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
mod imp {
    pub fn start(_: std::path::PathBuf, _: std::time::Duration, _: std::time::Duration) {
        eprintln!("crusteo: CRUSTEO_PROFILE is only supported on Windows x64");
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{self, BufWriter, Write};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind,
        SYMBOL_INFOW, SYMOPT_DEFERRED_LOADS, SYMOPT_UNDNAME, SymAddrIncludeInlineTrace,
        SymFromAddrW, SymFromInlineContextW, SymInitializeW, SymQueryInlineTrace, SymSetOptions,
        UNW_FLAG_NHANDLER,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, ResumeThread, SuspendThread,
    };
    use windows_sys::core::BOOL;

    const MAX_FRAMES: usize = 192;
    /// No real stack is this deep; a walk that gets further has gone astray.
    const MAX_STACK_BYTES: u64 = 64 << 20;

    /// A thread handle the sampler thread owns.
    struct Target(HANDLE);
    // Safety: a thread handle is a process-wide kernel handle.
    unsafe impl Send for Target {}

    pub fn start(path: PathBuf, delay: Duration, length: Duration) {
        let mut handle: HANDLE = std::ptr::null_mut();
        // GetCurrentThread is a pseudo-handle meaning "the caller"; duplicate
        // it into a real one the sampler thread can use.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentThread(),
                GetCurrentProcess(),
                &mut handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 {
            eprintln!("crusteo: profiler: could not open the thread to sample");
            return;
        }
        let target = Target(handle);
        let spawned = std::thread::Builder::new()
            .name("crusteo-sampler".into())
            .spawn(move || {
                let target = target;
                std::thread::sleep(delay);
                eprintln!(
                    "crusteo: profiler: sampling for {:.1}s",
                    length.as_secs_f64()
                );
                // 1ms sleeps instead of the default 15.6ms timer tick.
                unsafe { timeBeginPeriod(1) };
                let stacks = sample(&target, length);
                unsafe {
                    timeEndPeriod(1);
                    CloseHandle(target.0);
                }
                let total: u64 = stacks.values().sum();
                match write(&path, &stacks, total, length) {
                    Ok(distinct) => eprintln!(
                        "crusteo: profiler: {total} samples, {distinct} distinct stacks -> {}",
                        path.display()
                    ),
                    Err(err) => {
                        eprintln!(
                            "crusteo: profiler: writing {} failed: {err}",
                            path.display()
                        )
                    }
                }
            });
        if let Err(err) = spawned {
            eprintln!("crusteo: profiler: could not start: {err}");
        }
    }

    /// Stacks as addresses, leaf first, with how often each was seen.
    fn sample(target: &Target, length: Duration) -> HashMap<Vec<u64>, u64> {
        let mut stacks: HashMap<Vec<u64>, u64> = HashMap::new();
        let mut frames = [0u64; MAX_FRAMES];
        let end = Instant::now() + length;
        while Instant::now() < end {
            // Safety: the handle stays open until sampling ends.
            let n = unsafe { walk(target.0, &mut frames) };
            if n > 0 {
                *stacks.entry(frames[..n].to_vec()).or_default() += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        stacks
    }

    /// Suspends `thread`, unwinds its stack into `frames`, and resumes it.
    /// Nothing here allocates while the thread is suspended, since it may
    /// hold the heap lock.
    unsafe fn walk(thread: HANDLE, frames: &mut [u64; MAX_FRAMES]) -> usize {
        if unsafe { SuspendThread(thread) } == u32::MAX {
            return 0;
        }
        let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
        ctx.ContextFlags = CONTEXT_FULL_AMD64;
        let mut n = 0;
        if unsafe { GetThreadContext(thread, &mut ctx) } != 0 {
            let bottom = ctx.Rsp;
            while n < MAX_FRAMES && ctx.Rip != 0 {
                frames[n] = ctx.Rip;
                n += 1;
                // Stacks grow down, so every frame sits above the one it called.
                if ctx.Rsp < bottom
                    || ctx.Rsp - bottom > MAX_STACK_BYTES
                    || !ctx.Rsp.is_multiple_of(8)
                {
                    break;
                }
                let mut image_base = 0u64;
                let entry = unsafe {
                    RtlLookupFunctionEntry(ctx.Rip, &mut image_base, std::ptr::null_mut())
                };
                if entry.is_null() {
                    // A leaf function: its return address is on top of the stack.
                    ctx.Rip = unsafe { *(ctx.Rsp as *const u64) };
                    ctx.Rsp += 8;
                } else {
                    let mut handler_data: *mut c_void = std::ptr::null_mut();
                    let mut establisher = 0u64;
                    unsafe {
                        RtlVirtualUnwind(
                            UNW_FLAG_NHANDLER,
                            image_base,
                            ctx.Rip,
                            entry,
                            &mut ctx,
                            &mut handler_data,
                            &mut establisher,
                            std::ptr::null_mut(),
                        )
                    };
                }
            }
        }
        unsafe { ResumeThread(thread) };
        n
    }

    /// Writes folded stacks, root first. Returns how many lines it wrote.
    fn write(
        path: &Path,
        stacks: &HashMap<Vec<u64>, u64>,
        total: u64,
        length: Duration,
    ) -> io::Result<usize> {
        let process = unsafe { GetCurrentProcess() };
        // dbghelp does not look beside the executable unless told to, and
        // that is where cargo puts its .pdb.
        let search: Vec<u16> = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.as_os_str().to_owned()))
            .map(|dir| {
                std::os::windows::ffi::OsStrExt::encode_wide(dir.as_os_str())
                    .chain([0])
                    .collect()
            })
            .unwrap_or_else(|| vec![0]);
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
            SymInitializeW(process, search.as_ptr(), 1);
        }
        let mut names: HashMap<u64, Vec<String>> = HashMap::new();
        let mut out = BufWriter::new(File::create(path)?);
        writeln!(
            out,
            "# crusteo profiler: {total} wall-clock samples of the UI thread over {:.1}s",
            length.as_secs_f64()
        )?;
        let mut lines: Vec<(String, u64)> = Vec::with_capacity(stacks.len());
        for (stack, count) in stacks {
            let mut frames: Vec<String> = Vec::with_capacity(stack.len() * 2);
            for (i, &address) in stack.iter().enumerate() {
                // Return addresses point past the call; look up the call itself.
                let lookup = if i == 0 { address } else { address - 1 };
                let resolved = names
                    .entry(lookup)
                    .or_insert_with(|| symbolize(process, lookup));
                frames.extend(resolved.iter().cloned());
            }
            frames.reverse();
            lines.push((frames.join(";"), *count));
        }
        lines.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for (line, count) in &lines {
            writeln!(out, "{line} {count}")?;
        }
        out.flush()?;
        Ok(lines.len())
    }

    /// The function at `address`, innermost inlined frame first.
    fn symbolize(process: HANDLE, address: u64) -> Vec<String> {
        let mut names = Vec::new();
        let inlined = unsafe { SymAddrIncludeInlineTrace(process, address) };
        if inlined > 0 {
            let (mut context, mut frame) = (0u32, 0u32);
            let found = unsafe {
                SymQueryInlineTrace(
                    process,
                    address,
                    0,
                    address,
                    address,
                    &mut context,
                    &mut frame,
                )
            };
            if found != 0 {
                for i in 0..inlined {
                    if let Some(name) = symbol(|info, displacement| unsafe {
                        SymFromInlineContextW(process, address, context + i, displacement, info)
                    }) {
                        names.push(name);
                    }
                }
            }
        }
        let outer = symbol(|info, displacement| unsafe {
            SymFromAddrW(process, address, displacement, info)
        });
        names.push(outer.unwrap_or_else(|| format!("{address:#x}")));
        names
    }

    /// Calls a dbghelp lookup with room for a long name, returning the name.
    fn symbol(lookup: impl FnOnce(*mut SYMBOL_INFOW, *mut u64) -> BOOL) -> Option<String> {
        const MAX_NAME: usize = 1024;
        // SYMBOL_INFOW ends in a one-element name array; the name runs on past it.
        let words = (std::mem::size_of::<SYMBOL_INFOW>() + MAX_NAME * 2).div_ceil(8);
        let mut buffer = vec![0u64; words];
        let info = buffer.as_mut_ptr().cast::<SYMBOL_INFOW>();
        unsafe {
            (*info).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
            (*info).MaxNameLen = MAX_NAME as u32;
        }
        let mut displacement = 0u64;
        if lookup(info, &mut displacement) == 0 {
            return None;
        }
        let name = unsafe {
            let len = ((*info).NameLen as usize).min(MAX_NAME);
            std::slice::from_raw_parts(std::ptr::addr_of!((*info).Name).cast::<u16>(), len)
        };
        // Folded stacks separate frames with ';'.
        Some(String::from_utf16_lossy(name).replace(';', ","))
    }
}
