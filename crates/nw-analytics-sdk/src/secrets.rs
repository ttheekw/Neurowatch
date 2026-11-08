//! Small private state files. Unix uses owner-only files; Windows uses DPAPI
//! bound to the caller-supplied purpose. Secret values never enter config APIs.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

pub fn read(path: &Path, purpose: &[u8]) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    validate(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other("secret file too large"));
    }
    #[cfg(windows)]
    {
        dpapi(&bytes, purpose, false)
    }
    #[cfg(not(windows))]
    {
        let _ = purpose;
        Ok(bytes)
    }
}
fn validate(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a private state file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
    }
    Ok(())
}
pub fn write(path: &Path, purpose: &[u8], bytes: &[u8], replace: bool) -> io::Result<()> {
    if bytes.len() > 32768 {
        return Err(io::Error::other("secret too large"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing state directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(parent)?;
    let token = crate::identity::challenge().map_err(io::Error::other)?;
    let temporary = parent.join(format!(".analytics-{token}.tmp"));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(windows)]
        let stored = dpapi(bytes, purpose, true)?;
        #[cfg(not(windows))]
        let stored = {
            let _ = purpose;
            bytes.to_vec()
        };
        file.write_all(&stored)?;
        file.sync_all()?;
        drop(file);
        if replace {
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                let wide = |path: &Path| {
                    path.as_os_str()
                        .encode_wide()
                        .chain(Some(0))
                        .collect::<Vec<_>>()
                };
                if unsafe {
                    windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                        wide(&temporary).as_ptr(),
                        wide(path).as_ptr(),
                        1 | 8,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            #[cfg(not(windows))]
            std::fs::rename(&temporary, path)?;
        } else {
            std::fs::hard_link(&temporary, path)?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}
pub fn signing_key(path: &Path) -> io::Result<crate::identity::SigningKey> {
    const PURPOSE: &[u8] = b"NeuroWatch analytics plugin identity v1";
    match read(path, PURPOSE) {
        Ok(bytes) => Ok(crate::identity::SigningKey::from_bytes(
            &bytes
                .try_into()
                .map_err(|_| io::Error::other("invalid identity key"))?,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut bytes = [0; 32];
            getrandom::getrandom(&mut bytes).map_err(io::Error::other)?;
            match write(path, PURPOSE, &bytes, false) {
                Ok(()) => Ok(crate::identity::SigningKey::from_bytes(&bytes)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => signing_key(path),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}
#[cfg(windows)]
fn dpapi(bytes: &[u8], purpose: &[u8], protect: bool) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let blob = |bytes: &[u8]| CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &blob(bytes),
                std::ptr::null(),
                &blob(purpose),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &blob(bytes),
                std::ptr::null_mut(),
                &blob(purpose),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = if output.pbData.is_null() {
        vec![]
    } else {
        unsafe {
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            LocalFree(output.pbData.cast());
            bytes
        }
    };
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_survives_reopen_and_replacement_is_explicit() {
        let dir = std::env::temp_dir().join(format!("nw-sdk-secrets-{}", std::process::id()));
        let path = dir.join("plugin.key");
        let a = signing_key(&path).unwrap();
        let b = signing_key(&path).unwrap();
        assert_eq!(a.to_bytes(), b.to_bytes());
        assert!(write(&path, b"test", b"bad", false).is_err());
        assert_eq!(signing_key(&path).unwrap().to_bytes(), a.to_bytes());
        write(&path, b"test", b"bad", true).unwrap();
        assert!(signing_key(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
