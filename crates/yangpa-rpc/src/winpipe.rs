//! Windows named pipe 서버 확인.
//!
//! 기본 pipe 이름(`\\.\pipe\yangpad-<사용자 이름>`)은 남이 짐작할 수 있습니다. 같은 컴퓨터의 다른 사용자가 내 데몬보다
//! 먼저 그 이름으로 pipe를 만들어 두면 내 앱·CLI가 그쪽에 붙어 `auth.hello`로 소유자 토큰을, 이어서 메시지·기기 연결 링크를
//! 보내게 됩니다. 그래서 토큰을 보내기 전에 pipe 서버 프로세스가 나와 같은 OS 사용자(토큰의 사용자 SID)인지 봅니다.
//! Unix 소켓은 0700 디렉터리 안에 있어 이런 가로채기가 없습니다.
//!
//! 서버 쪽 pipe는 내 사용자와 SYSTEM만 열 수 있게 만듭니다([`private_pipe_descriptor`]).

use std::io;
use std::os::windows::io::AsRawHandle;

use interprocess::local_socket::tokio::Stream as LocalStream;
use interprocess::local_socket::traits::StreamCommon;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::U16CString;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 직접 연 named pipe 클라이언트(`connect_pipe`)의 서버가 이 프로세스와 같은 사용자의 프로세스인지 확인합니다.
pub(crate) fn verify_pipe_server(pipe: &impl AsRawHandle) -> io::Result<()> {
    let mut pid: u32 = 0;
    // SAFETY: 살아 있는 pipe 클라이언트의 핸들을 빌려 쓰기만 하고 닫지 않습니다. `pid`는 유효한 u32 포인터입니다.
    let ok = unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    verify_server_pid(pid)
}

/// interprocess로 연 연결의 서버가 이 프로세스와 같은 사용자의 프로세스인지 확인합니다.
pub(crate) fn verify_server(stream: &LocalStream) -> io::Result<()> {
    let pid = stream
        .peer_creds()?
        .pid()
        .ok_or_else(|| io::Error::other("pipe 서버의 프로세스 ID를 알 수 없습니다"))?;
    verify_server_pid(pid)
}

/// 프로세스 `pid`가 이 프로세스와 같은 사용자 SID로 도는지. 확인할 수 없으면 거부합니다.
pub(crate) fn verify_server_pid(pid: u32) -> io::Result<()> {
    let me = process_user_sid(std::process::id())?;
    let server = process_user_sid(pid).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "데몬 pipe 서버(PID {pid})의 사용자를 확인할 수 없어 연결하지 않습니다({e}). \
                 관리자 권한으로 띄운 데몬이면 CLI도 같은 권한으로 실행하세요"
            ),
        )
    })?;
    if server == me {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "데몬 pipe 서버(PID {pid})가 다른 OS 사용자의 프로세스라 연결하지 않습니다. \
                 다른 사용자가 같은 이름의 pipe를 먼저 열었을 수 있습니다"
            ),
        ))
    }
}

/// 이 프로세스의 사용자와 SYSTEM만 열 수 있는 pipe 보안 설명자. 설명자 없이 만들면 Windows 기본값이 Everyone·Anonymous에게
/// 읽기를 주어, 같은 컴퓨터의 다른 계정이 인증하지 못하는 읽기 핸들만으로 연결 자리를 모두 차지해 내 앱·CLI가 붙지 못합니다.
pub(crate) fn private_pipe_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = sid_string(&process_user_sid(std::process::id())?)?;
    let sddl = U16CString::from_str(pipe_sddl(&sid)).map_err(io::Error::other)?;
    SecurityDescriptor::deserialize(&sddl)
}

/// 상속을 막고(`P`) 그 사용자와 SYSTEM에게만 모든 권한을 주는 DACL.
fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)")
}

/// 이진 SID를 `S-1-5-21-…` 문자열로.
fn sid_string(sid: &[u8]) -> io::Result<String> {
    // SAFETY: `sid`는 process_user_sid가 돌려준 올바른 SID 바이트이고 API는 읽기만 합니다. 받은 문자열은 NUL로 끝나며, 길이를
    // 센 뒤 복사하고 LocalFree로 한 번만 놓습니다.
    unsafe {
        let mut out = std::ptr::null_mut::<u16>();
        if ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut out) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0usize;
        while *out.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
        LocalFree(out.cast());
        Ok(s)
    }
}

/// 닫는 것을 잊지 않게 감싼 핸들.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OpenProcess·OpenProcessToken이 돌려준 유효한 핸들이고 여기서 한 번만 닫습니다.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// 프로세스 토큰의 사용자 SID(이진 표현). SID는 정규형이라 바이트 비교가 `EqualSid`와 같습니다.
fn process_user_sid(pid: u32) -> io::Result<Vec<u8>> {
    // SAFETY: 아래 호출은 모두 Win32 문서의 계약대로 씁니다. 핸들은 OwnedHandle이 닫고, TOKEN_USER는 GetTokenInformation이
    // 알려 준 길이 이상이며 포인터 정렬(8바이트)을 맞춘 버퍼에 받습니다. SID 포인터는 그 버퍼 안을 가리키고 버퍼가 살아
    // 있는 동안만 읽습니다.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle(process);
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process.0, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut len: u32 = 0;
        // 첫 호출은 필요한 길이만 알려 주고 실패합니다(ERROR_INSUFFICIENT_BUFFER).
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len);
        if (len as usize) < std::mem::size_of::<TOKEN_USER>() {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let sid = user.User.Sid;
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Err(io::Error::other("토큰의 사용자 SID가 올바르지 않습니다"));
        }
        let n = GetLengthSid(sid) as usize;
        Ok(std::slice::from_raw_parts(sid.cast::<u8>(), n).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn pipe_descriptor_names_only_this_user_and_system() {
        use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt;
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
        let sid = sid_string(&process_user_sid(std::process::id()).unwrap()).unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        // 만든 설명자를 다시 문자열로 풀어 보면 Everyone(WD)·Anonymous(AN) 없이 이 사용자와 SYSTEM만 있습니다.
        let dacl = private_pipe_descriptor()
            .unwrap()
            .serialize(DACL_SECURITY_INFORMATION, |s| s.to_string_lossy())
            .unwrap();
        // 기본 관리자 계정(RID 500, GitHub 러너의 runneradmin)이면 Windows가 SID를 약칭 `LA`로 풀어 씁니다.
        let user = if sid.ends_with("-500") {
            ";;;LA)"
        } else {
            &sid
        };
        assert!(dacl.contains(user) && dacl.contains(";;;SY)"), "{dacl}");
        assert!(
            !dacl.contains(";;;WD)") && !dacl.contains(";;;AN)"),
            "{dacl}"
        );
    }

    #[test]
    fn own_process_is_accepted() {
        verify_server_pid(std::process::id()).unwrap();
        assert!(!process_user_sid(std::process::id()).unwrap().is_empty());
    }

    #[test]
    fn same_user_child_process_is_accepted() {
        // 표준 입력을 기다리는 동안만 사는 자식(시간에 기대지 않음). 같은 사용자의 다른 프로세스라 SID가 같아야 합니다.
        let mut child = Command::new("cmd")
            .args(["/D", "/C", "set /p X="])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd 실행");
        let verified = verify_server_pid(child.id());
        let sid = process_user_sid(child.id());
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"\r\n");
        }
        let _ = child.wait();
        verified.unwrap();
        assert_eq!(sid.unwrap(), process_user_sid(std::process::id()).unwrap());
    }

    #[tokio::test]
    async fn direct_pipe_to_own_server_is_accepted() {
        // client.rs의 connect_pipe처럼 tokio로 직접 연 pipe도 서버 PID를 알아내 확인해야 합니다.
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        let name = format!(r"\\.\pipe\yangpa-winpipe-test-{}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("pipe 서버");
        let client = ClientOptions::new().open(&name).expect("pipe 연결");
        verify_pipe_server(&client).unwrap();
        drop(server);
    }

    #[test]
    fn other_user_or_missing_process_is_rejected() {
        // PID 4는 System(SYSTEM 사용자 또는 열 수 없음), 0은 유휴 프로세스라 열 수 없습니다.
        for pid in [0u32, 4] {
            let e = verify_server_pid(pid).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "PID {pid}: {e}");
        }
    }
}
