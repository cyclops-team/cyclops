//! One bounded protocol writer shared by tool replies and channel notices.
//!
//! A host that stops reading stdout must not strand the channel thread during
//! teardown. A partial or timed-out frame closes output instead of appending
//! another JSON object to bytes whose delivery is unknown.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

struct Output {
    file: File,
    failed: bool,
    timeout: Duration,
}

#[derive(Clone)]
pub(super) struct SharedOutput(Arc<Mutex<Output>>);

impl SharedOutput {
    pub(super) fn stdout() -> io::Result<Self> {
        // This command owns stdout for its process lifetime. O_NONBLOCK also
        // applies to the original descriptor; no other stdout writer is used.
        let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::new(unsafe { File::from_raw_fd(fd) }, FRAME_TIMEOUT)
    }

    fn new(file: File, timeout: Duration) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Arc::new(Mutex::new(Output {
            file,
            failed: false,
            timeout,
        }))))
    }
}

impl Write for SharedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut output = self
            .0
            .lock()
            .map_err(|_| io::Error::other("MCP output lock poisoned"))?;
        if output.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "MCP output is closed",
            ));
        }
        let deadline = Instant::now() + output.timeout;
        let result = write_until(&mut output.file, bytes, deadline);
        output.failed = result.is_err();
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        // File writes are unbuffered; write_all is the frame boundary.
        Ok(())
    }
}

fn write_until(file: &mut File, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MCP host stopped reading stdout",
            ));
        }
        match file.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "MCP stdout closed",
                ))
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let mut ready = libc::pollfd {
                    fd: file.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
                if unsafe { libc::poll(&mut ready, 1, timeout) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn backpressured_output() -> (SharedOutput, File) {
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let read = unsafe { File::from_raw_fd(fds[0]) };
    let write = unsafe { File::from_raw_fd(fds[1]) };
    let output = SharedOutput::new(write, Duration::from_millis(100)).unwrap();
    {
        let mut held = output.0.lock().unwrap();
        loop {
            match held.file.write(&[0; 4096]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                result => panic!("fill output pipe: {result:?}"),
            }
        }
    }
    (output, read)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_stdout_times_out_and_cannot_receive_another_frame() {
        let (mut output, _reader) = backpressured_output();
        assert_eq!(
            output.write_all(b"notice\n").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            output.write_all(b"later\n").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
