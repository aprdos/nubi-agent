//! Process hardening applied first thing at startup.
//!
//! nubi-agent keeps the agent's long-lived credentials (`NUBI_TOKEN`,
//! `NUBI_HEARTBEAT_TOKEN`, `NUBI_LLM_KEY`) in its environment. In a managed
//! sandbox it runs as the same user as the coding CLI and its shell, so any
//! command the CLI runs could otherwise read them from `/proc/<pid>/environ`
//! or `/proc/<pid>/mem`, or attach with ptrace. A non-dumpable process has its
//! `/proc/<pid>` files owned by root and refuses ptrace from its own user.
//!
//! `execve` resets the flag, so it does not reach the processes nubi-agent
//! starts: those get a cleared environment instead (see `harness`).

/// Marks this process non-dumpable. Never fails the start: if the kernel
/// refuses, it says so on stderr (tracing is not set up yet) and carries on.
pub fn deny_same_user_inspection() {
    if let Err(e) = set_non_dumpable() {
        eprintln!(
            "nubi-agent: could not mark the process non-dumpable (prctl PR_SET_DUMPABLE): {e}; \
             processes of the same user can read its environment"
        );
    }
}

#[cfg(target_os = "linux")]
fn set_non_dumpable() -> std::io::Result<()> {
    // The argument is read as an unsigned long: pass one, not an int, through
    // the variadic call.
    // SAFETY: PR_SET_DUMPABLE only reads its integer argument.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Elsewhere (macOS, where people run it on their own machine) there is no
/// such flag; nothing to do.
#[cfg(not(target_os = "linux"))]
fn set_non_dumpable() -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn deny_same_user_inspection_makes_the_process_non_dumpable() {
        deny_same_user_inspection();
        // SAFETY: PR_GET_DUMPABLE takes no argument and returns the flag.
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        assert_eq!(dumpable, 0);

        // Calling it again is harmless.
        deny_same_user_inspection();
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }, 0);

        // What the agent's shell sees: /proc/<pid>/environ no longer belongs
        // to the user both run as. (As root it would, so nothing to check.)
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            let owner = std::fs::metadata(format!("/proc/{}/environ", std::process::id()))
                .expect("stat /proc/<pid>/environ")
                .uid();
            assert_ne!(owner, euid);
        }
    }
}
