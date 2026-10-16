//! The FreeBSD backend: OpenPAM (`/etc/pam.d/jos-greet`) authenticates
//! and opens the session; the desktop is started in a child that calls
//! `setusercontext(3)` (groups, login class, umask, uid: what login(1)
//! does) before exec'ing the plan from [`crate::launch`].
//!
//! All `unsafe` is in this file and limited to the libpam/libutil calls
//! and the memory PAM hands back; every pointer comes from a live
//! `CString`/`Box` owned by [`Pam`] or from PAM itself.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};

use juanitos_core::sys::Runner;

use crate::greeter::Power;
use crate::gui::{shutdown, Backend};
use crate::launch::{self, Account, PAM_SERVICE};
use crate::sessions::Session;

// security/pam_constants.h
const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 6;
const PAM_NEW_AUTHTOK_REQD: c_int = 10;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_MAX_NUM_MSG: c_int = 32;
const PAM_ESTABLISH_CRED: c_int = 0x1;
const PAM_DELETE_CRED: c_int = 0x2;
const PAM_TTY: c_int = 3;
// login_cap.h: everything but the environment, which the child gets
// from the plan.
const LOGIN_SETALL: c_uint = 0x07ff;
const LOGIN_SETPATH: c_uint = 0x0004;
const LOGIN_SETENV: c_uint = 0x0080;
const USER_CONTEXT: c_uint = LOGIN_SETALL & !(LOGIN_SETPATH | LOGIN_SETENV);

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

type ConvFn = unsafe extern "C" fn(
    c_int,
    *mut *const PamMessage,
    *mut *mut PamResponse,
    *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

#[repr(C)]
struct PamHandle {
    _opaque: [u8; 0],
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_end(pamh: *mut PamHandle, status: c_int) -> c_int;
    fn pam_set_item(pamh: *mut PamHandle, item_type: c_int, item: *const c_void) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_open_session(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_close_session(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_getenvlist(pamh: *mut PamHandle) -> *mut *mut c_char;
    fn pam_strerror(pamh: *mut PamHandle, errnum: c_int) -> *const c_char;
}

#[link(name = "util")]
extern "C" {
    fn setusercontext(
        lc: *mut c_void,
        pwd: *const libc::passwd,
        uid: libc::uid_t,
        flags: c_uint,
    ) -> c_int;
}

/// What the conversation function answers with.
struct ConvData {
    user: CString,
    password: Option<CString>,
    /// Errors/notices PAM modules sent (shown when authentication fails).
    messages: Vec<String>,
}

fn wipe_cstring(s: CString) {
    let mut bytes = s.into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

/// `strdup` of a Rust C string into malloc'd memory PAM will free.
fn dup(s: &CStr) -> *mut c_char {
    // SAFETY: `s` is a valid NUL-terminated string.
    unsafe { libc::strdup(s.as_ptr()) }
}

unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if !(1..=PAM_MAX_NUM_MSG).contains(&num_msg) || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    // SAFETY: `appdata` is the `ConvData` box registered in `Pam::start`,
    // alive for as long as the handle.
    let Some(data) = (unsafe { (appdata as *mut ConvData).as_mut() }) else {
        return PAM_CONV_ERR;
    };
    // SAFETY: OpenPAM passes an array of `num_msg` message pointers.
    let msgs = unsafe { std::slice::from_raw_parts(msg, n) };
    // SAFETY: zeroed responses (null `resp` strings) are what PAM expects
    // for messages that need no answer; PAM frees the array with free(3).
    let out = unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if out.is_null() {
        return PAM_BUF_ERR;
    }
    // SAFETY: `out` holds `n` zeroed responses.
    let answers = unsafe { std::slice::from_raw_parts_mut(out, n) };
    let mut ok = true;
    for (m, a) in msgs.iter().zip(answers.iter_mut()) {
        // SAFETY: each message pointer is valid for the call.
        let Some(m) = (unsafe { m.as_ref() }) else {
            ok = false;
            break;
        };
        let text = if m.msg.is_null() {
            String::new()
        } else {
            // SAFETY: PAM messages are NUL-terminated strings.
            unsafe { CStr::from_ptr(m.msg) }
                .to_string_lossy()
                .trim()
                .to_string()
        };
        match m.msg_style {
            PAM_PROMPT_ECHO_OFF => match &data.password {
                Some(p) => a.resp = dup(p),
                None => ok = false,
            },
            PAM_PROMPT_ECHO_ON => a.resp = dup(&data.user),
            PAM_ERROR_MSG | PAM_TEXT_INFO => {
                if !text.is_empty() {
                    data.messages.push(text);
                }
            }
            _ => ok = false,
        }
        if !ok {
            break;
        }
    }
    if !ok {
        for a in answers.iter_mut() {
            if !a.resp.is_null() {
                // SAFETY: allocated by strdup above.
                unsafe { libc::free(a.resp as *mut c_void) };
            }
        }
        // SAFETY: allocated by calloc above.
        unsafe { libc::free(out as *mut c_void) };
        return PAM_CONV_ERR;
    }
    // SAFETY: `resp` is PAM's out-parameter.
    unsafe { *resp = out };
    PAM_SUCCESS
}

/// A PAM transaction for one login.
struct Pam {
    handle: *mut PamHandle,
    data: Box<ConvData>,
    _conv: Box<PamConv>,
    session_open: bool,
    cred_established: bool,
}

impl Pam {
    fn start(user: &str, password: &str, tty: &str) -> Result<Pam, String> {
        let service = CString::new(PAM_SERVICE).map_err(|e| e.to_string())?;
        let user_c = CString::new(user).map_err(|_| "Login incorrect".to_string())?;
        let password = CString::new(password).map_err(|_| "Login incorrect".to_string())?;
        let mut data = Box::new(ConvData {
            user: user_c.clone(),
            password: Some(password),
            messages: Vec::new(),
        });
        let conv = Box::new(PamConv {
            conv: Some(conversation),
            appdata_ptr: (&mut *data) as *mut ConvData as *mut c_void,
        });
        let mut handle: *mut PamHandle = std::ptr::null_mut();
        // SAFETY: all pointers are live for the call; `conv` and `data`
        // are boxed and kept in the returned `Pam` (stable addresses).
        let rc = unsafe { pam_start(service.as_ptr(), user_c.as_ptr(), &*conv, &mut handle) };
        if rc != PAM_SUCCESS || handle.is_null() {
            return Err(format!("PAM service {PAM_SERVICE} unavailable ({rc})"));
        }
        let pam = Pam {
            handle,
            data,
            _conv: conv,
            session_open: false,
            cred_established: false,
        };
        let tty = CString::new(tty).map_err(|e| e.to_string())?;
        // SAFETY: PAM copies string items.
        unsafe { pam_set_item(pam.handle, PAM_TTY, tty.as_ptr() as *const c_void) };
        Ok(pam)
    }

    fn error(&self, rc: c_int) -> String {
        // SAFETY: pam_strerror returns a static string (or null).
        let s = unsafe { pam_strerror(self.handle, rc) };
        if s.is_null() {
            return format!("PAM error {rc}");
        }
        // SAFETY: non-null, NUL-terminated.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    fn forget_password(&mut self) {
        if let Some(p) = self.data.password.take() {
            wipe_cstring(p);
        }
    }

    fn authenticate(&mut self) -> Result<(), String> {
        // SAFETY: valid handle.
        let rc = unsafe { pam_authenticate(self.handle, 0) };
        self.forget_password();
        if rc != PAM_SUCCESS {
            return Err("Login incorrect".to_string());
        }
        // SAFETY: valid handle.
        let rc = unsafe { pam_acct_mgmt(self.handle, 0) };
        match rc {
            PAM_SUCCESS => Ok(()),
            PAM_NEW_AUTHTOK_REQD => {
                Err("Password expired: change it on the text console (Alt+F1)".to_string())
            }
            _ => Err(self
                .data
                .messages
                .last()
                .cloned()
                .unwrap_or_else(|| self.error(rc))),
        }
    }

    fn open_session(&mut self) -> Result<Vec<String>, String> {
        // SAFETY: valid handle.
        let rc = unsafe { pam_setcred(self.handle, PAM_ESTABLISH_CRED) };
        if rc != PAM_SUCCESS {
            return Err(format!("Cannot establish credentials: {}", self.error(rc)));
        }
        self.cred_established = true;
        // SAFETY: valid handle.
        let rc = unsafe { pam_open_session(self.handle, 0) };
        if rc != PAM_SUCCESS {
            return Err(format!("Cannot open a session: {}", self.error(rc)));
        }
        self.session_open = true;
        Ok(self.env())
    }

    fn env(&self) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: valid handle; the list and its strings are malloc'd
        // copies the caller frees.
        let list = unsafe { pam_getenvlist(self.handle) };
        if list.is_null() {
            return out;
        }
        let mut p = list;
        loop {
            // SAFETY: the list is NULL-terminated.
            let s = unsafe { *p };
            if s.is_null() {
                break;
            }
            // SAFETY: NUL-terminated string from PAM, freed right after.
            out.push(unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned());
            unsafe { libc::free(s as *mut c_void) };
            // SAFETY: still inside the NULL-terminated list.
            p = unsafe { p.add(1) };
        }
        // SAFETY: the array itself is malloc'd.
        unsafe { libc::free(list as *mut c_void) };
        out
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        self.forget_password();
        // SAFETY: valid handle, ended exactly once here.
        unsafe {
            if self.session_open {
                pam_close_session(self.handle, 0);
            }
            if self.cred_established {
                pam_setcred(self.handle, PAM_DELETE_CRED);
            }
            pam_end(self.handle, PAM_SUCCESS);
        }
    }
}

fn lookup(user: &str) -> Option<Account> {
    let name = CString::new(user).ok()?;
    // SAFETY: zeroed passwd is a valid out-param; `buf` outlives the
    // copies made below.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    let s = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: getpwnam_r fills NUL-terminated strings into `buf`.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    Some(Account {
        name: s(pwd.pw_name),
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home: s(pwd.pw_dir),
        shell: s(pwd.pw_shell),
    })
}

/// `/var/run/user/<uid>`, owned by the user, mode 700.
fn prepare_runtime_dir(a: &Account) -> Result<(), String> {
    let dir = a.runtime_dir();
    let fail = |e: std::io::Error| format!("{}: {e}", dir.display());
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if !m.is_dir() || m.uid() != a.uid => {
            return Err(format!("{} is not {}'s directory", dir.display(), a.name));
        }
        Ok(_) => {}
        Err(_) => {
            std::fs::create_dir_all(&dir).map_err(fail)?;
            std::os::unix::fs::chown(&dir, Some(a.uid), Some(a.gid)).map_err(fail)?;
        }
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)
}

fn open_log(a: &Account) -> Result<File, String> {
    let path = a.session_log();
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let f = File::create(&path).map_err(fail)?;
    std::os::unix::fs::chown(&path, Some(a.uid), Some(a.gid)).map_err(fail)?;
    Ok(f)
}

pub struct FreeBsdBackend<'a> {
    pub runner: &'a dyn Runner,
    pub vt: i32,
    pending: Option<(Pam, Account)>,
}

impl<'a> FreeBsdBackend<'a> {
    pub fn new(runner: &'a dyn Runner, vt: i32) -> Self {
        FreeBsdBackend {
            runner,
            vt,
            pending: None,
        }
    }
}

impl Backend for FreeBsdBackend<'_> {
    fn authenticate(&mut self, user: &str, password: &str) -> Result<(), String> {
        self.pending = None;
        let mut pam = Pam::start(user, password, &launch::tty_name(self.vt))?;
        pam.authenticate()?;
        let account = lookup(user).ok_or_else(|| "Login incorrect".to_string())?;
        account.check()?;
        self.pending = Some((pam, account));
        Ok(())
    }

    fn run_session(&mut self, user: &str, session: &Session) -> Result<(), String> {
        let Some((mut pam, account)) = self.pending.take() else {
            return Err("not authenticated".to_string());
        };
        if account.name != user {
            return Err("not authenticated".to_string());
        }
        let pam_env = pam.open_session()?;
        prepare_runtime_dir(&account)?;
        let log = open_log(&account)?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;
        let plan = launch::plan(&account, session, self.vt, &pam_env);
        let name = CString::new(account.name.clone()).map_err(|e| e.to_string())?;

        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.args)
            .env_clear()
            .envs(plan.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&plan.cwd)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err);
        // SAFETY: runs in the forked child of this single-threaded process
        // before exec, like login(1): new session, then the user's
        // context (groups, class limits, umask, setlogin, uid).
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let pw = libc::getpwnam(name.as_ptr());
                if pw.is_null() {
                    return Err(std::io::Error::other("user vanished"));
                }
                if setusercontext(std::ptr::null_mut(), pw, (*pw).pw_uid, USER_CONTEXT) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let log_path = account.session_log().display().to_string();
        let status = cmd
            .spawn()
            .and_then(|mut child| child.wait())
            .map_err(|e| format!("Cannot start {}: {e}", session.name))?;
        drop(pam);
        launch::exit_message(&session.name, status.code(), &log_path).map_err(|e| {
            match status.signal() {
                Some(sig) => format!("{e} [signal {sig}]"),
                None => e,
            }
        })
    }

    fn power(&mut self, p: Power) -> anyhow::Result<()> {
        shutdown(self.runner, p)
    }
}
