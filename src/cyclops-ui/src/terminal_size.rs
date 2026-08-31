//! The terminal cell size shared by workspace construction and watch.

use std::mem::MaybeUninit;

pub(crate) fn get() -> (usize, usize) {
    unsafe {
        let mut size = MaybeUninit::<libc::winsize>::uninit();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) == 0 {
            let size = size.assume_init();
            if size.ws_col > 0 && size.ws_row > 0 {
                return (size.ws_col as usize, size.ws_row as usize);
            }
        }
    }
    (80, 24)
}
