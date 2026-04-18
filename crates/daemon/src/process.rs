use std::io;

use tokio::process::Command;

pub fn configure_command_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

#[cfg(unix)]
pub fn kill_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(_pid: u32, _signal: libc::c_int) -> io::Result<()> {
    Ok(())
}
