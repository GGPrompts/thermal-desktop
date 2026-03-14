//! PAM-based password authentication via direct FFI.
//!
//! We call libpam directly rather than using the `pam` crate (which requires
//! bindgen/clang to build).  The PAM application API is stable and simple.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use tracing::warn;
use zeroize::Zeroizing;

// ── PAM FFI types ─────────────────────────────────────────────────────────────

#[repr(C)]
struct PamHandle {
    _opaque: [u8; 0],
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type PamConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: PamConvFn,
    appdata_ptr: *mut c_void,
}

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;

unsafe extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;

    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;

    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;

    fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;

    fn malloc(size: usize) -> *mut c_void;
    fn calloc(nmemb: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

// ── Conversation callback ─────────────────────────────────────────────────────

/// Pointer to the password string passed through `appdata_ptr`.
unsafe extern "C" fn pam_conv_callback(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    unsafe {
        let password_ptr = appdata_ptr as *const c_char;

        // Allocate response array (zeroed)
        let n = num_msg as usize;
        let responses =
            calloc(n, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if responses.is_null() {
            return -1; // PAM_BUF_ERR
        }

        for i in 0..n {
            let message = &**msg.add(i);
            let response = &mut *responses.add(i);
            response.resp_retcode = 0;

            if message.msg_style == PAM_PROMPT_ECHO_OFF {
                // Duplicate the password string into a malloc'd buffer PAM can free
                let pwd_str = CStr::from_ptr(password_ptr);
                let len = pwd_str.to_bytes_with_nul().len();
                let copy = malloc(len) as *mut c_char;
                if copy.is_null() {
                    free(responses as *mut c_void);
                    return -1;
                }
                std::ptr::copy_nonoverlapping(password_ptr, copy, len);
                response.resp = copy;
            } else {
                response.resp = std::ptr::null_mut();
            }
        }

        *resp = responses;
        PAM_SUCCESS
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Authenticate `username` with `password` via PAM service "login".
/// Returns `true` on success.
pub fn authenticate(username: &str, password: &Zeroizing<String>) -> bool {
    let service = match CString::new("login") {
        Ok(s) => s,
        Err(e) => {
            warn!("PAM: invalid service name: {}", e);
            return false;
        }
    };
    let user_c = match CString::new(username) {
        Ok(s) => s,
        Err(e) => {
            warn!("PAM: invalid username: {}", e);
            return false;
        }
    };
    let pass_c = match CString::new(password.as_str()) {
        Ok(s) => s,
        Err(e) => {
            warn!("PAM: invalid password: {}", e);
            return false;
        }
    };

    let result = unsafe {
        let conv = PamConv {
            conv: pam_conv_callback,
            appdata_ptr: pass_c.as_ptr() as *mut c_void,
        };

        let mut pamh: *mut PamHandle = std::ptr::null_mut();
        let ret = pam_start(service.as_ptr(), user_c.as_ptr(), &conv, &mut pamh);
        if ret != PAM_SUCCESS {
            warn!("PAM: pam_start failed with code {}", ret);
            return false;
        }

        let auth_ret = pam_authenticate(pamh, 0);
        // Check account validity (expired/locked accounts, policy restrictions)
        let acct_ret = if auth_ret == PAM_SUCCESS {
            pam_acct_mgmt(pamh, 0)
        } else {
            auth_ret
        };
        pam_end(pamh, acct_ret);

        if acct_ret != PAM_SUCCESS {
            warn!("PAM: authentication failed with code {}", acct_ret);
            false
        } else {
            true
        }
    };

    // Zero the CString bytes before dropping
    unsafe {
        let ptr = pass_c.as_ptr() as *mut u8;
        let len = pass_c.as_bytes().len();
        std::ptr::write_bytes(ptr, 0, len);
    }

    result
}

/// Get the current user's login name.
///
/// Uses `nix::unistd::User::from_uid(getuid())` to read the passwd entry,
/// falling back to the `USER` env var, then "user".
pub fn current_username() -> String {
    use nix::unistd::{getuid, User};
    match User::from_uid(getuid()) {
        Ok(Some(user)) => user.name,
        Ok(None) => {
            warn!("PAM: no passwd entry for current uid");
            std::env::var("USER").unwrap_or_else(|_| "user".to_string())
        }
        Err(e) => {
            warn!("PAM: getpwuid error: {}", e);
            std::env::var("USER").unwrap_or_else(|_| "user".to_string())
        }
    }
}
