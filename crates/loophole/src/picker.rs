use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use ashpd::desktop::ResponseError;
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};

pub async fn open_file(title: &str, filter: FileFilter) -> Result<Option<PathBuf>> {
    let request = SelectedFiles::open_file()
        .title(title)
        .modal(true)
        .multiple(false)
        .filter(filter)
        .send()
        .await
        .context("no file chooser (xdg-desktop-portal), type the path instead")?;
    let files = match request.response() {
        Ok(files) => files,
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(files.uris().first().and_then(|uri| file_path(uri.as_str())))
}

fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest).as_bytes();
    if path.first() != Some(&b'/') {
        return None;
    }
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let escaped = (path[i] == b'%')
            .then(|| path.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        if let Some(byte) = escaped {
            out.push(byte);
            i += 3;
        } else {
            out.push(path[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}
