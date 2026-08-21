use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Errno(pub u16);

impl Errno {
    pub const ENOENT: Errno = Errno(libc::ENOENT as u16);
    pub const EIO: Errno = Errno(libc::EIO as u16);
    pub const ESTALE: Errno = Errno(libc::ESTALE as u16);
    pub const ENOSYS: Errno = Errno(libc::ENOSYS as u16);
    pub const EACCES: Errno = Errno(libc::EACCES as u16);
    pub const EINVAL: Errno = Errno(libc::EINVAL as u16);
    pub const ENODATA: Errno = Errno(libc::ENODATA as u16);
    pub const ERANGE: Errno = Errno(libc::ERANGE as u16);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY as u16);
    pub const EEXIST: Errno = Errno(libc::EEXIST as u16);

    pub fn from_io(e: &std::io::Error) -> Errno {
        match e.raw_os_error() {
            Some(code) if (1..=4095).contains(&code) => Errno(code as u16),
            _ => Errno::EIO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_io_error_to_errno() {
        let e = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(Errno::from_io(&e), Errno::ENOENT);
    }

    #[test]
    fn unknown_io_error_becomes_eio() {
        let e = std::io::Error::other("no os code");
        assert_eq!(Errno::from_io(&e), Errno::EIO);
    }
}
