// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Windows **virtual files** model (OLE): an `IDataObject` that promises a
//! remote FILES clip through `CFSTR_FILEDESCRIPTORW` (names + sizes, free, from
//! the frozen manifest) and `CFSTR_FILECONTENTS` (one `IStream` PER file, whose
//! bytes are pulled from the peer on demand). This is the destination-side
//! counterpart of the classic `CF_HDROP` path in [`crate::windows`] and the
//! Windows analogue of the Linux FUSE mount ([`crate::fuse`]) and the macOS
//! `NSFilePresenter` fill: a paste materializes nothing up front, and each range
//! Explorer reads triggers one on-demand pull.
//!
//! # COM objects (`#[implement]`, AGILE by default)
//!
//! - [`FilesDataObject`] : `IDataObject`. `GetData` serves the group descriptor
//!   (an `HGLOBAL`) or, for a given `lindex`, a [`PullStream`] (`TYMED_ISTREAM`).
//! - [`PullStream`] : a read-only, sequential `IStream` whose `Read` pulls the
//!   next range straight from a [`FileFetcher`] (see the range-native note
//!   below); every other method refuses cleanly.
//! - [`FormatEnumerator`] : `IEnumFORMATETC` over the two announced formats.
//!
//! Because the objects are AGILE (the free-threaded marshaler, the `#[implement]`
//! default), Explorer calls `IStream::Read` on ITS OWN copy thread (the progress
//! bar) — never on our message loop. Hence the `Send + Sync` requirement: no raw
//! pointer is kept (the enumerator stores plain `(cfFormat, tymed)` pairs and
//! rebuilds `FORMATETC` on the fly), and mutable state lives behind a `Mutex`.
//! `Arc<dyn FileFetcher>` is `Send + Sync + 'static` by its trait bound, so it
//! rides along safely.
//!
//! # Range-native divergence from the clipnet reference
//!
//! The private POC this is ported from drained a sequential reader with a
//! skip-and-discard cache. We DON'T: [`FileFetcher::read`] is already range-
//! addressed, so [`PullStream`] simply asks for `[read_so_far, read_so_far+cb)`
//! and advances. There is no reader object and no reader cache.
//!
//! # Limitation
//!
//! Empty directories are NOT listed: the descriptor carries only files, and
//! Explorer recreates the intermediate folders from each file's relative path.
//! A directory with no files under it therefore does not appear (same as the
//! reference; acceptable — the file tree is the payload).
//!
//! Real cross-process delivery (COM marshaling to Explorer) is NOT testable off
//! a real Windows desktop: this module is cross-compiled and covered by the
//! in-process unit tests below; the end-to-end validation is manual.

use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_NOTIMPL, GlobalFree, HGLOBAL,
    OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA, ISequentialStream_Impl, IStream,
    IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::{
    FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
};
use windows::core::{BOOL, HRESULT, PCWSTR, Ref, Result as WinResult, implement};

use crate::backend::{FileFetcher, RemoteFile};

/// A `&str` → a `NUL`-terminated UTF-16 string for the `…W` APIs.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One FILE entry of the offer (directories are filtered out): its manifest
/// `file_id` (for the fetcher), its relative path already `\`-separated (for the
/// descriptor), and its size (for the descriptor + `IStream::Stat`).
struct FileEntry {
    file_id: String,
    rel_path_backslash: String,
    size: u64,
}

/// Register the two virtual-files clipboard formats and build the `IDataObject`
/// for a remote FILES clip: the descriptor comes from the manifest, the contents
/// are pulled through `fetcher` on demand (never read here). Only non-directory
/// `files` entries are carried; their `path` (relative, `/`-separated) is
/// converted to the `\`-separated form Windows expects.
pub fn build_files_data_object(
    files: &[RemoteFile],
    fetcher: Arc<dyn FileFetcher>,
) -> WinResult<IDataObject> {
    // SAFETY: `RegisterClipboardFormatW` only reads the NUL-terminated string we
    // pass; it returns a process-global format id (or 0 on failure).
    let cf_descriptor =
        unsafe { RegisterClipboardFormatW(PCWSTR(wide_z("FileGroupDescriptorW").as_ptr())) } as u16;
    let cf_contents =
        unsafe { RegisterClipboardFormatW(PCWSTR(wide_z("FileContents").as_ptr())) } as u16;
    if cf_descriptor == 0 || cf_contents == 0 {
        return Err(E_FAIL.into());
    }
    // Order = descriptor order = the `lindex` Explorer will hand back for
    // contents. Directories are implied by the file subpaths (see the module
    // limitation), so they are dropped here.
    let entries: Vec<FileEntry> = files
        .iter()
        .filter(|f| !f.dir)
        .map(|f| FileEntry {
            file_id: f.file_id.clone(),
            rel_path_backslash: f.path.replace('/', "\\"),
            size: f.size,
        })
        .collect();
    let obj = FilesDataObject {
        cf_descriptor,
        cf_contents,
        files: entries,
        fetcher,
    };
    Ok(obj.into())
}

// ===================== IStream: read-only, pulling ranges on demand =========

/// A read-only, sequential `IStream` whose bytes are pulled from the peer one
/// range at a time. Range-native: `Read` asks the fetcher for exactly the window
/// Explorer requested, so there is no sequential reader to drain.
#[implement(IStream)]
struct PullStream {
    fetcher: Arc<dyn FileFetcher>,
    file_id: String,
    /// Declared size (manifest) — for `Stat` / the progress bar.
    size: u64,
    /// Bytes handed out so far = the next pull offset. Behind a `Mutex` because
    /// the object is agile and `Read` runs on Explorer's copy thread.
    read_so_far: Mutex<u64>,
}

impl PullStream {
    fn new(fetcher: Arc<dyn FileFetcher>, file_id: String, size: u64) -> Self {
        Self {
            fetcher,
            file_id,
            size,
            read_so_far: Mutex::new(0),
        }
    }
}

impl ISequentialStream_Impl for PullStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_FAIL;
        }
        let mut read_so_far = self.read_so_far.lock().unwrap_or_else(|p| p.into_inner());
        // A COM method must NEVER unwind across the `extern "system"` vtable edge
        // (windows-rs generates no catch there): on our pinned toolchain that
        // aborts the process, and Explorer calls this on ITS OWN copy thread. The
        // fetcher does a `block_on` into the IPC runtime, which can panic if that
        // runtime is being torn down while a copy is still in flight (a shutdown
        // race). Contain any panic and turn it into the SAME clean refusal as a
        // failed pull — never a silent truncation, never an abort. `read_so_far`
        // is not advanced on that path (nothing was read).
        let offset = *read_so_far;
        let pulled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.fetcher.read(&self.file_id, offset, cb as u64)
        }));
        match pulled {
            Ok(Ok(bytes)) => {
                // The FileFetcher contract already caps the range at `cb`, but a
                // buggy fetcher must NEVER let us overrun the caller's buffer:
                // clamp defensively.
                let n = bytes.len().min(cb as usize);
                // SAFETY: Explorer guarantees `pv` addresses `cb` writable bytes;
                // we copy only the first `n <= cb`.
                let dst = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
                dst[..n].copy_from_slice(&bytes[..n]);
                *read_so_far += n as u64;
                if !pcbread.is_null() {
                    // SAFETY: `pcbread`, when non-null, is a valid `*mut u32`.
                    unsafe { *pcbread = n as u32 };
                }
                // S_OK even at EOF (n == 0): Explorer stops when *pcbread == 0.
                S_OK
            }
            // A failed pull OR a contained panic is a clean refusal, NEVER a
            // silent truncation and NEVER an unwind across the COM boundary.
            Ok(Err(_)) | Err(_) => {
                if !pcbread.is_null() {
                    // SAFETY: as above.
                    unsafe { *pcbread = 0 };
                }
                E_FAIL
            }
        }
    }

    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED // read-only stream
    }
}

impl IStream_Impl for PullStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> WinResult<()> {
        // Sequential stream: tolerate only querying the position (`Seek(0, CUR)`)
        // and a no-op seek to the current position. No rewind — Explorer reads
        // strictly forward and needs no more.
        let pos = *self.read_so_far.lock().unwrap_or_else(|p| p.into_inner());
        let querying_current = dworigin == STREAM_SEEK_CUR && dlibmove == 0;
        let seek_to_current =
            dworigin == STREAM_SEEK_SET && dlibmove >= 0 && dlibmove as u64 == pos;
        if !(querying_current || seek_to_current) {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        if !plibnewposition.is_null() {
            // SAFETY: `plibnewposition`, when non-null, is a valid `*mut u64`.
            unsafe { *plibnewposition = pos };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> WinResult<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _pstm: Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Revert(&self) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> WinResult<()> {
        if pstatstg.is_null() {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        // STATSTG is POD (no Drop): an all-zero value is valid (null PWSTR/GUID).
        // SAFETY: zeroed STATSTG is a valid bit pattern; we then set the fields
        // Explorer needs and write it into the caller's out-pointer.
        let mut st: STATSTG = unsafe { core::mem::zeroed() };
        st.r#type = STGTY_STREAM.0 as u32;
        st.cbSize = self.size;
        unsafe { core::ptr::write(pstatstg, st) };
        Ok(())
    }

    fn Clone(&self) -> WinResult<IStream> {
        // A pull stream is single-consumption: not clonable.
        Err(E_NOTIMPL.into())
    }
}

// ===================== IDataObject: descriptor + on-demand contents =========

/// The `IDataObject` of a streamed-on-demand remote FILES clip.
#[implement(IDataObject)]
struct FilesDataObject {
    cf_descriptor: u16,
    cf_contents: u16,
    files: Vec<FileEntry>,
    fetcher: Arc<dyn FileFetcher>,
}

impl FilesDataObject {
    /// Build the `FILEGROUPDESCRIPTORW` (cItems + one `FILEDESCRIPTORW` per file)
    /// into a movable `HGLOBAL`, transferred as-is to the consumer (who frees it).
    fn build_group_descriptor(&self) -> WinResult<HGLOBAL> {
        let n = self.files.len();
        // `FILEGROUPDESCRIPTORW` already embeds one descriptor (`fgd: [_; 1]`);
        // append the remaining (n-1) after it.
        let total = std::mem::size_of::<FILEGROUPDESCRIPTORW>()
            + n.saturating_sub(1) * std::mem::size_of::<FILEDESCRIPTORW>();
        // SAFETY: GlobalAlloc(GMEM_MOVEABLE, total) returns a movable handle or
        // an error we propagate; GlobalLock then pins it for the writes below.
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, total)? };
        let base = unsafe { GlobalLock(hglobal) } as *mut FILEGROUPDESCRIPTORW;
        if base.is_null() {
            // GlobalLock only pins an already-committed block, so on a fresh
            // movable handle this is effectively unreachable — but if it ever did
            // fail, we still own `hglobal` (no STGMEDIUM escapes on this branch to
            // carry it to the consumer), so free it rather than orphan it.
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            return Err(E_FAIL.into());
        }
        // SAFETY: `base` addresses `total` bytes we just allocated. Both
        // FILEGROUPDESCRIPTORW and FILEDESCRIPTORW are `#[repr(C, packed(1))]`
        // (alignment 1), so every `fds.add(i)` is a valid write target; we go
        // through `addr_of_mut!` to avoid forming a reference to a packed field.
        unsafe {
            (*base).cItems = n as u32;
            let fds = core::ptr::addr_of_mut!((*base).fgd) as *mut FILEDESCRIPTORW;
            for (i, entry) in self.files.iter().enumerate() {
                let mut cfile = [0u16; 260];
                // Truncate to 259 wchars so the NUL terminator always fits.
                for (slot, w) in cfile
                    .iter_mut()
                    .zip(entry.rel_path_backslash.encode_utf16().take(259))
                {
                    *slot = w;
                }
                let fd = FILEDESCRIPTORW {
                    dwFlags: (FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
                    nFileSizeLow: (entry.size & 0xFFFF_FFFF) as u32,
                    nFileSizeHigh: (entry.size >> 32) as u32,
                    cFileName: cfile,
                    ..Default::default()
                };
                core::ptr::write(fds.add(i), fd);
            }
            let _ = GlobalUnlock(hglobal);
        }
        Ok(hglobal)
    }

    /// A `STGMEDIUM` wrapping an `HGLOBAL` (ownership transferred to the consumer).
    fn medium_hglobal(hglobal: HGLOBAL) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    /// A `STGMEDIUM` wrapping an `IStream` (ownership transferred to the consumer).
    fn medium_stream(stream: IStream) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }
}

impl IDataObject_Impl for FilesDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> WinResult<STGMEDIUM> {
        // SAFETY: OLE passes a valid `*const FORMATETC`; treat null defensively.
        let fe =
            unsafe { pformatetcin.as_ref() }.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        if fe.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT.into());
        }
        if fe.cfFormat == self.cf_descriptor {
            if fe.tymed & (TYMED_HGLOBAL.0 as u32) == 0 {
                return Err(DV_E_TYMED.into());
            }
            let h = self.build_group_descriptor()?;
            Ok(FilesDataObject::medium_hglobal(h))
        } else if fe.cfFormat == self.cf_contents {
            if fe.tymed & (TYMED_ISTREAM.0 as u32) == 0 {
                return Err(DV_E_TYMED.into());
            }
            // `lindex` = the file's index in the descriptor.
            let lindex = fe.lindex;
            if lindex < 0 || lindex as usize >= self.files.len() {
                return Err(DV_E_LINDEX.into());
            }
            let entry = &self.files[lindex as usize];
            let stream: IStream =
                PullStream::new(self.fetcher.clone(), entry.file_id.clone(), entry.size).into();
            Ok(FilesDataObject::medium_stream(stream))
        } else {
            Err(DV_E_FORMATETC.into())
        }
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> WinResult<()> {
        // We do not fill a caller-provided medium (our contents are our streams).
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: OLE passes a valid `*const FORMATETC`; treat null defensively.
        let fe = match unsafe { pformatetc.as_ref() } {
            Some(fe) => fe,
            None => return E_FAIL,
        };
        if fe.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        if fe.cfFormat == self.cf_descriptor {
            if fe.tymed & (TYMED_HGLOBAL.0 as u32) != 0 {
                S_OK
            } else {
                DV_E_TYMED
            }
        } else if fe.cfFormat == self.cf_contents {
            if fe.tymed & (TYMED_ISTREAM.0 as u32) != 0 {
                S_OK
            } else {
                DV_E_TYMED
            }
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        _pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        // No canonicalization: the caller uses the format as-is.
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into()) // read-only object
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
        if dwdirection == DATADIR_GET.0 as u32 {
            let entries = vec![
                (self.cf_descriptor, TYMED_HGLOBAL.0 as u32),
                (self.cf_contents, TYMED_ISTREAM.0 as u32),
            ];
            Ok(FormatEnumerator::new(entries).into())
        } else {
            Err(E_NOTIMPL.into()) // no write formats (SetData is refused)
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

// ===================== IEnumFORMATETC =======================================

/// Enumerates the offered formats. Stores PLAIN data `(cfFormat, tymed)` (never a
/// raw `FORMATETC`, which carries a pointer → `!Send`) and rebuilds `FORMATETC`
/// on the fly, so the object stays `Send + Sync` (required, as it is agile).
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    entries: Vec<(u16, u32)>,
    pos: Mutex<usize>,
}

impl FormatEnumerator {
    fn new(entries: Vec<(u16, u32)>) -> Self {
        Self {
            entries,
            pos: Mutex::new(0),
        }
    }
}

/// Build a content-aspect `FORMATETC` (`lindex = -1`: the caller supplies the
/// real index in `GetData`).
fn formatetc(cf_format: u16, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf_format,
        ptd: core::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed,
    }
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        if rgelt.is_null() {
            return E_FAIL;
        }
        let mut pos = self.pos.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: OLE guarantees `rgelt` addresses `celt` writable FORMATETC.
        let out = unsafe { std::slice::from_raw_parts_mut(rgelt, celt as usize) };
        let mut fetched = 0usize;
        while fetched < celt as usize && *pos < self.entries.len() {
            let (cf, tymed) = self.entries[*pos];
            out[fetched] = formatetc(cf, tymed);
            *pos += 1;
            fetched += 1;
        }
        if !pceltfetched.is_null() {
            // SAFETY: `pceltfetched`, when non-null, is a valid `*mut u32`.
            unsafe { *pceltfetched = fetched as u32 };
        }
        if fetched == celt as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, celt: u32) -> WinResult<()> {
        let mut pos = self.pos.lock().unwrap_or_else(|p| p.into_inner());
        let new = *pos + celt as usize;
        if new > self.entries.len() {
            *pos = self.entries.len();
            return Err(S_FALSE.into());
        }
        *pos = new;
        Ok(())
    }

    fn Reset(&self) -> WinResult<()> {
        *self.pos.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumFORMATETC> {
        let pos = *self.pos.lock().unwrap_or_else(|p| p.into_inner());
        let cloned = FormatEnumerator {
            entries: self.entries.clone(),
            pos: Mutex::new(pos),
        };
        Ok(cloned.into())
    }
}

#[cfg(test)]
mod tests {
    //! In-process COM tests: they build the `IDataObject` DIRECTLY and call it on
    //! the same thread (no marshaling → no apartment needed), so they compile and
    //! run in CI on windows-latest. The real cross-process delivery to Explorer
    //! is untestable here (see the module docs) and is validated manually.
    use std::collections::HashMap;
    use std::io;

    use windows::Win32::System::Com::{IStream, STGMEDIUM};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::ReleaseStgMedium;

    use super::*;

    /// A deterministic fetcher: byte at absolute offset `k` is `(k % 251)`. It
    /// knows each file's size (to compute EOF) and can be forced to fail.
    struct FakeFetcher {
        sizes: HashMap<String, u64>,
        fail: bool,
        panic: bool,
    }

    fn byte_at(k: u64) -> u8 {
        (k % 251) as u8
    }

    impl FileFetcher for FakeFetcher {
        fn read(&self, file_id: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
            if self.panic {
                panic!("forced fetcher panic (test)");
            }
            if self.fail {
                return Err(io::Error::other("forced pull failure"));
            }
            let size = *self
                .sizes
                .get(file_id)
                .ok_or_else(|| io::Error::other("unknown file_id"))?;
            if offset >= size {
                return Ok(Vec::new()); // EOF
            }
            let end = (offset + len).min(size);
            Ok((offset..end).map(byte_at).collect())
        }
    }

    /// The two-file manifest used across the tests: a top-level file and a file in
    /// a subdirectory, plus a directory entry that must be filtered out.
    fn sample_files() -> Vec<RemoteFile> {
        vec![
            RemoteFile {
                file_id: "id-a".into(),
                path: "a.txt".into(),
                size: 10,
                dir: false,
            },
            RemoteFile {
                file_id: "id-dir".into(),
                path: "sub".into(),
                size: 0,
                dir: true,
            },
            RemoteFile {
                file_id: "id-b".into(),
                path: "sub/b.bin".into(),
                size: 600,
                dir: false,
            },
        ]
    }

    fn fetcher(fail: bool) -> Arc<dyn FileFetcher> {
        let mut sizes = HashMap::new();
        sizes.insert("id-a".to_string(), 10u64);
        sizes.insert("id-b".to_string(), 600u64);
        Arc::new(FakeFetcher {
            sizes,
            fail,
            panic: false,
        })
    }

    /// A fetcher whose `read` panics — to prove the COM `Read` boundary contains
    /// it (a COM method must never unwind).
    fn panicking_fetcher() -> Arc<dyn FileFetcher> {
        Arc::new(FakeFetcher {
            sizes: HashMap::new(),
            fail: false,
            panic: true,
        })
    }

    fn cf_ids() -> (u16, u16) {
        let d = unsafe { RegisterClipboardFormatW(PCWSTR(wide_z("FileGroupDescriptorW").as_ptr())) }
            as u16;
        let c = unsafe { RegisterClipboardFormatW(PCWSTR(wide_z("FileContents").as_ptr())) } as u16;
        (d, c)
    }

    fn fe(cf: u16, tymed: u32, aspect: u32, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: core::ptr::null_mut(),
            dwAspect: aspect,
            lindex,
            tymed,
        }
    }

    /// Read an `IStream` to EOF using a deliberately small, odd `cb` so partial
    /// reads and the mid-stream advance are exercised.
    fn read_stream_fully(stream: &IStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            let mut got: u32 = 0;
            let hr = unsafe {
                stream.Read(
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                    Some(&mut got),
                )
            };
            assert_eq!(hr, S_OK, "Read must return S_OK, got {hr:?}");
            if got == 0 {
                break;
            }
            out.extend_from_slice(&buf[..got as usize]);
        }
        out
    }

    #[test]
    fn query_get_data_accepts_the_two_formats_and_rejects_the_rest() {
        let (cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");

        let t_h = TYMED_HGLOBAL.0 as u32;
        let t_s = TYMED_ISTREAM.0 as u32;
        let content = DVASPECT_CONTENT.0;

        // Descriptor + HGLOBAL and contents + ISTREAM are accepted.
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(cf_d, t_h, content, -1)) },
            S_OK
        );
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(cf_c, t_s, content, 0)) },
            S_OK
        );
        // Wrong tymed for each.
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(cf_d, t_s, content, -1)) },
            DV_E_TYMED
        );
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(cf_c, t_h, content, 0)) },
            DV_E_TYMED
        );
        // Unknown format id.
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(0xFFFF, t_h, content, -1)) },
            DV_E_FORMATETC
        );
        // Wrong aspect.
        assert_eq!(
            unsafe { obj.QueryGetData(&fe(cf_d, t_h, content + 1, -1)) },
            DV_E_DVASPECT
        );
    }

    #[test]
    fn enum_format_etc_yields_the_two_entries_then_s_false() {
        let (cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");
        let enumr = unsafe { obj.EnumFormatEtc(DATADIR_GET.0 as u32) }.expect("enum");

        let mut buf = [formatetc(0, 0), formatetc(0, 0)];
        let mut fetched: u32 = 0;
        let hr = unsafe { enumr.Next(&mut buf, Some(&mut fetched)) };
        assert_eq!(hr, S_OK);
        assert_eq!(fetched, 2);
        assert_eq!(buf[0].cfFormat, cf_d);
        assert_eq!(buf[0].tymed, TYMED_HGLOBAL.0 as u32);
        assert_eq!(buf[1].cfFormat, cf_c);
        assert_eq!(buf[1].tymed, TYMED_ISTREAM.0 as u32);

        // Exhausted → S_FALSE, nothing fetched.
        let mut fetched2: u32 = 0;
        let hr2 = unsafe { enumr.Next(&mut buf, Some(&mut fetched2)) };
        assert_eq!(hr2, S_FALSE);
        assert_eq!(fetched2, 0);
    }

    #[test]
    fn get_data_descriptor_carries_the_filtered_files() {
        let (cf_d, _cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");

        let mut medium: STGMEDIUM =
            unsafe { obj.GetData(&fe(cf_d, TYMED_HGLOBAL.0 as u32, DVASPECT_CONTENT.0, -1)) }
                .expect("GetData descriptor");
        assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);

        let hglobal = unsafe { medium.u.hGlobal };
        let base = unsafe { GlobalLock(hglobal) } as *const FILEGROUPDESCRIPTORW;
        assert!(!base.is_null());
        let (c_items, name0, lo0, hi0, flags0, name1, lo1, hi1) = unsafe {
            let c_items = (*base).cItems;
            let fds = core::ptr::addr_of!((*base).fgd) as *const FILEDESCRIPTORW;
            let fd0 = core::ptr::read(fds.add(0));
            let fd1 = core::ptr::read(fds.add(1));
            // `cFileName` must be copied out BY VALUE first: FILEDESCRIPTORW is
            // `#[repr(C, packed(1))]`, so a *reference* to the field would be an
            // unaligned-reference error. An array read is a plain copy.
            (
                c_items,
                fd_name(fd0.cFileName),
                fd0.nFileSizeLow,
                fd0.nFileSizeHigh,
                fd0.dwFlags,
                fd_name(fd1.cFileName),
                fd1.nFileSizeLow,
                fd1.nFileSizeHigh,
            )
        };
        let _ = unsafe { GlobalUnlock(hglobal) };

        assert_eq!(c_items, 2, "the directory entry must be filtered out");
        assert_eq!(name0, "a.txt");
        assert_eq!((lo0, hi0), (10, 0));
        assert_eq!(name1, "sub\\b.bin", "the subpath uses backslashes");
        assert_eq!((lo1, hi1), (600, 0));
        let want = (FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32;
        assert_eq!(flags0, want);

        // Release the transferred medium (tymed HGLOBAL, no pUnkForRelease).
        unsafe { ReleaseStgMedium(&mut medium) };
    }

    /// Extract the NUL-terminated name out of a `cFileName` array (taken by value
    /// — the source field lives in a packed struct, so it must be copied out).
    fn fd_name(name: [u16; 260]) -> String {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf16_lossy(&name[..len])
    }

    #[test]
    fn get_data_contents_streams_the_fake_bytes_at_each_lindex() {
        let (_cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");

        for (lindex, size) in [(0i32, 10u64), (1i32, 600u64)] {
            let mut medium = unsafe {
                obj.GetData(&fe(
                    cf_c,
                    TYMED_ISTREAM.0 as u32,
                    DVASPECT_CONTENT.0,
                    lindex,
                ))
            }
            .expect("GetData contents");
            assert_eq!(medium.tymed, TYMED_ISTREAM.0 as u32);
            // Take ownership of the stream out of the medium (POD, no Drop).
            let stream = unsafe { ManuallyDrop::take(&mut medium.u.pstm) }.expect("stream present");
            let bytes = read_stream_fully(&stream);
            assert_eq!(bytes.len() as u64, size, "total must equal the file size");
            let expected: Vec<u8> = (0..size).map(byte_at).collect();
            assert_eq!(
                bytes, expected,
                "streamed bytes must match the fake pattern"
            );
        }
    }

    #[test]
    fn get_data_contents_rejects_an_out_of_range_lindex() {
        let (_cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");
        // `STGMEDIUM` is not `Debug`, so match rather than `expect_err`.
        let result =
            unsafe { obj.GetData(&fe(cf_c, TYMED_ISTREAM.0 as u32, DVASPECT_CONTENT.0, 2)) };
        let err = match result {
            Ok(_) => panic!("lindex 2 is out of range (only 0,1 exist) and must be rejected"),
            Err(e) => e,
        };
        assert_eq!(err.code(), DV_E_LINDEX);
    }

    #[test]
    fn stream_read_advances_across_small_reads() {
        let (_cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(false)).expect("build");
        let mut medium =
            unsafe { obj.GetData(&fe(cf_c, TYMED_ISTREAM.0 as u32, DVASPECT_CONTENT.0, 0)) }
                .expect("GetData contents");
        let stream = unsafe { ManuallyDrop::take(&mut medium.u.pstm) }.expect("stream present");

        // First a 4-byte read (offsets 0..4), then a 4-byte read (offsets 4..8):
        // the second must continue where the first left off.
        let mut buf = [0u8; 4];
        let mut got = 0u32;
        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                4,
                Some(&mut got),
            )
        };
        assert_eq!(hr, S_OK);
        assert_eq!(got, 4);
        assert_eq!(buf, [byte_at(0), byte_at(1), byte_at(2), byte_at(3)]);

        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                4,
                Some(&mut got),
            )
        };
        assert_eq!(hr, S_OK);
        assert_eq!(got, 4);
        assert_eq!(buf, [byte_at(4), byte_at(5), byte_at(6), byte_at(7)]);
    }

    #[test]
    fn stream_read_returns_e_fail_on_a_failed_pull() {
        let (_cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), fetcher(true)).expect("build");
        let mut medium =
            unsafe { obj.GetData(&fe(cf_c, TYMED_ISTREAM.0 as u32, DVASPECT_CONTENT.0, 0)) }
                .expect("GetData contents");
        let stream = unsafe { ManuallyDrop::take(&mut medium.u.pstm) }.expect("stream present");

        let mut buf = [0u8; 8];
        let mut got: u32 = 123;
        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                Some(&mut got),
            )
        };
        assert_eq!(hr, E_FAIL, "a failed pull must be a clean refusal");
        assert_eq!(got, 0, "*pcbread must be 0 on failure");
    }

    #[test]
    fn stream_read_contains_a_fetcher_panic_as_e_fail() {
        let (_cf_d, cf_c) = cf_ids();
        let obj = build_files_data_object(&sample_files(), panicking_fetcher()).expect("build");
        let mut medium =
            unsafe { obj.GetData(&fe(cf_c, TYMED_ISTREAM.0 as u32, DVASPECT_CONTENT.0, 0)) }
                .expect("GetData contents");
        let stream = unsafe { ManuallyDrop::take(&mut medium.u.pstm) }.expect("stream present");

        // Silence the default panic hook around the deliberate, contained panic
        // (it would otherwise print an alarming "thread panicked" line for a
        // panic we EXPECT and catch). Restored immediately after.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut buf = [0u8; 8];
        let mut got: u32 = 123;
        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                Some(&mut got),
            )
        };
        std::panic::set_hook(prev);

        // The panic must be contained as the same clean refusal a failed pull
        // yields — never an unwind across the COM boundary (which would abort).
        assert_eq!(hr, E_FAIL, "a fetcher panic must be contained as E_FAIL");
        assert_eq!(got, 0, "*pcbread must be 0 when the pull panics");
    }
}
