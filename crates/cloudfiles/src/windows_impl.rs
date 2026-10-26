#![allow(unsafe_code)] // the Cloud Files API is a C API

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::CloudFilters::{
    CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE_NONE, CF_CONNECT_FLAG_NONE, CF_CONNECTION_KEY,
    CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CONVERT_FLAG_NONE, CF_HARDLINK_POLICY_NONE,
    CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_ALWAYS_FULL, CF_HYDRATION_POLICY_MODIFIER_NONE,
    CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_INSYNC_POLICY_TRACK_ALL,
    CF_PIN_STATE_PINNED, CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT, CF_PLACEHOLDER_STATE_IN_SYNC,
    CF_PLACEHOLDER_STATE_PLACEHOLDER, CF_POPULATION_POLICY, CF_POPULATION_POLICY_ALWAYS_FULL,
    CF_POPULATION_POLICY_MODIFIER_NONE, CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT,
    CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT, CF_REGISTER_FLAG_UPDATE, CF_SET_IN_SYNC_FLAG_NONE,
    CF_SET_PIN_FLAG_NONE, CF_SYNC_POLICIES, CF_SYNC_REGISTRATION, CfConnectSyncRoot,
    CfConvertToPlaceholder, CfDisconnectSyncRoot, CfGetPlaceholderStateFromFileInfo,
    CfRegisterSyncRoot, CfSetInSyncState, CfSetPinState, CfUnregisterSyncRoot,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    READ_CONTROL, WRITE_DAC,
};
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::{Error, Result};

/// Identifies BetterSync as the provider of its sync roots.
const PROVIDER_ID: GUID = GUID::from_u128(0x6b1f_5c2e_8d3a_4e7b_9f10_2a4c_6e8d_b5c1);

fn os_err(what: &'static str, path: &Path, e: &windows::core::Error) -> Error {
    Error::Os {
        what,
        path: path.display().to_string(),
        message: e.message(),
    }
}

/// A synced folder registered and connected as a Cloud Files sync root.
/// Dropping it disconnects; the registration (and the badges) stay until
/// [`SyncRoot::unregister`].
#[derive(Debug)]
pub struct SyncRoot {
    root: PathBuf,
    key: CF_CONNECTION_KEY,
}

/// An open handle, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed only here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open(path: &Path) -> Result<Handle> {
    // WRITE_DAC is enough for the Cloud Files calls and, unlike write
    // access to the data, is not refused while another program (Word, an
    // editor) has the file open. The content is never touched.
    // SAFETY: valid NUL-terminated path; the handle is owned by `Handle`.
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(path.as_os_str()),
            (READ_CONTROL | WRITE_DAC).0 | (FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            // Directories need BACKUP_SEMANTICS; OPEN_REPARSE_POINT keeps
            // the cloud filter from treating this open as a read of data.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| os_err("opening", path, &e))?;
    Ok(Handle(h))
}

/// (is a placeholder, is in sync)
fn placeholder_state(h: &Handle, path: &Path) -> Result<(bool, bool)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is a FILE_ATTRIBUTE_TAG_INFO of the size passed.
    unsafe {
        GetFileInformationByHandleEx(
            h.0,
            FileAttributeTagInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|e| os_err("reading attributes of", path, &e))?;
    // SAFETY: `info` was filled in for FileAttributeTagInfo above.
    let state = unsafe {
        CfGetPlaceholderStateFromFileInfo((&raw const info).cast::<c_void>(), FileAttributeTagInfo)
    };
    Ok((
        state.0 & CF_PLACEHOLDER_STATE_PLACEHOLDER.0 != 0,
        state.0 & CF_PLACEHOLDER_STATE_IN_SYNC.0 != 0,
    ))
}

impl SyncRoot {
    /// Register `root` (an existing directory) as a sync root and connect
    /// to it. Registering again updates the registration.
    pub fn register(root: &Path, folder_id: &str) -> Result<Self> {
        let path = HSTRING::from(root.as_os_str());
        let name = HSTRING::from("BetterSync");
        let version = HSTRING::from(env!("CARGO_PKG_VERSION"));
        let identity = folder_id.as_bytes();
        let registration = CF_SYNC_REGISTRATION {
            StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
            ProviderName: PCWSTR(name.as_ptr()),
            ProviderVersion: PCWSTR(version.as_ptr()),
            SyncRootIdentity: identity.as_ptr().cast::<c_void>(),
            SyncRootIdentityLength: identity.len() as u32,
            FileIdentity: std::ptr::null(),
            FileIdentityLength: 0,
            ProviderId: PROVIDER_ID,
        };
        let policies = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            // Every file always has its full content: Windows refuses to
            // dehydrate (there is nobody to hydrate it again on demand).
            Hydration: CF_HYDRATION_POLICY {
                Primary: CF_HYDRATION_POLICY_ALWAYS_FULL,
                Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
            },
            Population: CF_POPULATION_POLICY {
                Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
                Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
            },
            // Any change by another program clears "in sync" (the badge
            // turns to "pending") until the daemon has synced it.
            InSync: CF_INSYNC_POLICY_TRACK_ALL,
            HardLink: CF_HARDLINK_POLICY_NONE,
            PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
        };
        let flags = CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT
            | CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT;
        // SAFETY: all pointers in `registration` outlive the calls.
        let registered = unsafe { CfRegisterSyncRoot(&path, &registration, &policies, flags) }
            .or_else(|_| unsafe {
                CfRegisterSyncRoot(
                    &path,
                    &registration,
                    &policies,
                    flags | CF_REGISTER_FLAG_UPDATE,
                )
            });
        registered.map_err(|e| os_err("registering sync root", root, &e))?;

        // No callbacks: with full population and hydration Windows never
        // needs to ask for anything.
        let callbacks = [CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        }];
        // SAFETY: the table is terminated by CF_CALLBACK_TYPE_NONE and has
        // no callbacks that could outlive it.
        let key =
            unsafe { CfConnectSyncRoot(&path, callbacks.as_ptr(), None, CF_CONNECT_FLAG_NONE) }
                .map_err(|e| os_err("connecting sync root", root, &e))?;
        Ok(Self {
            root: root.to_path_buf(),
            key,
        })
    }

    /// Show `path` (a file or directory under the root) as in sync or not.
    /// Returns whether anything changed; does nothing if the state is
    /// already right, so the change notifications this causes settle.
    pub fn set_in_sync(&self, path: &Path, in_sync: bool) -> Result<bool> {
        let h = open(path)?;
        let (placeholder, current) = placeholder_state(&h, path)?;
        if placeholder && current == in_sync {
            return Ok(false);
        }
        if placeholder {
            let state = if in_sync {
                CF_IN_SYNC_STATE_IN_SYNC
            } else {
                CF_IN_SYNC_STATE_NOT_IN_SYNC
            };
            // SAFETY: `h` is an open handle to `path`.
            unsafe { CfSetInSyncState(h.0, state, CF_SET_IN_SYNC_FLAG_NONE, None) }
                .map_err(|e| os_err("setting sync state of", path, &e))?;
        } else {
            let flags = if in_sync {
                CF_CONVERT_FLAG_MARK_IN_SYNC
            } else {
                CF_CONVERT_FLAG_NONE
            };
            // SAFETY: `h` is an open handle to `path`; no file identity.
            unsafe { CfConvertToPlaceholder(h.0, None, 0, flags, None, None) }
                .map_err(|e| os_err("converting", path, &e))?;
            // Belt and braces next to the hydration policy: "always keep
            // on this device".
            // SAFETY: as above.
            unsafe { CfSetPinState(h.0, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None) }
                .map_err(|e| os_err("pinning", path, &e))?;
        }
        Ok(true)
    }

    /// Remove the registration of `root`. Files stay as they are (full
    /// placeholders behave like ordinary files without a provider).
    pub fn unregister(root: &Path) -> Result<()> {
        // SAFETY: valid NUL-terminated path.
        unsafe { CfUnregisterSyncRoot(&HSTRING::from(root.as_os_str())) }
            .map_err(|e| os_err("unregistering sync root", root, &e))
    }
}

impl Drop for SyncRoot {
    fn drop(&mut self) {
        // SAFETY: the key came from CfConnectSyncRoot and is used once here.
        if let Err(e) = unsafe { CfDisconnectSyncRoot(self.key) } {
            let _ = os_err("disconnecting", &self.root, &e);
        }
    }
}
