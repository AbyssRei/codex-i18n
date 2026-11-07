//! Windows package, Authenticode and Shell integration.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, HANDLE, HWND, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows::Win32::Security::Cryptography::{CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW};
use windows::Win32::Security::WinTrust::*;
use windows::Win32::Storage::FileSystem::MoveFileW;
use windows::Win32::Storage::Packaging::Appx::{
    FindPackagesByPackageFamily, GetPackagePathByFullName, PACKAGE_FILTER_HEAD,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink,
};
use windows::core::{Interface, PCWSTR, PWSTR, w};

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn chatgpt_running() -> Result<bool> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        struct Guard(HANDLE);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_err() {
            bail!("无法检查正在运行的应用");
        }
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&x| x == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case("ChatGPT.exe") {
                return Ok(true);
            }
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                ensure!(
                    error.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0),
                    "检查运行中应用失败：{error}"
                );
                break;
            }
        }
        Ok(false)
    }
}

pub struct BuildLock(HANDLE);
impl Drop for BuildLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
pub fn build_lock() -> Result<BuildLock> {
    named_build_lock(w!("Local\\CodexI18nBuildLock"))
}
fn named_build_lock(name: PCWSTR) -> Result<BuildLock> {
    unsafe {
        let handle = CreateMutexW(None, false, name)?;
        let result = WaitForSingleObject(handle, 0);
        if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
            let _ = CloseHandle(handle);
            bail!("另一个管理器正在生成修复版，请等待其完成");
        }
        Ok(BuildLock(handle))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub full_name: String,
    pub version: [u16; 4],
    pub app_dir: PathBuf,
}

fn parse_version(full: &str) -> Result<[u16; 4]> {
    let mut parts = full.split('_');
    ensure!(parts.next() == Some("OpenAI.Codex"), "包名称不匹配");
    let v = parts.next().context("包版本缺失")?;
    let values = v
        .split('.')
        .map(str::parse::<u16>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    values
        .try_into()
        .map_err(|_| anyhow::anyhow!("包版本应为四段"))
}

pub fn find_package() -> Result<Package> {
    unsafe {
        let (mut count, mut size) = (0, 0);
        let family = w!("OpenAI.Codex_2p2nqsd0c76g0");
        let status = FindPackagesByPackageFamily(
            family,
            PACKAGE_FILTER_HEAD,
            &mut count,
            None,
            &mut size,
            None,
            None,
        );
        if count == 0 {
            bail!(
                "当前用户未安装 OpenAI.Codex，或无法查询安装包（{}）",
                status.0
            );
        }
        ensure!(
            status == ERROR_INSUFFICIENT_BUFFER,
            "查询安装包失败（{}）",
            status.0
        );
        let mut names = vec![PWSTR::null(); count as usize];
        let mut storage = vec![0u16; size as usize];
        let status = FindPackagesByPackageFamily(
            family,
            PACKAGE_FILTER_HEAD,
            &mut count,
            Some(names.as_mut_ptr()),
            &mut size,
            Some(PWSTR(storage.as_mut_ptr())),
            None,
        );
        ensure!(status.0 == 0, "查询安装包失败（{}）", status.0);
        ensure!(count == 1, "发现 {count} 个候选安装包，无法确定当前版本");
        let full_name = names[0].to_string().context("包名称无效")?;
        let mut path_size = 0;
        let status = GetPackagePathByFullName(PCWSTR(names[0].0), &mut path_size, None);
        ensure!(
            status == ERROR_INSUFFICIENT_BUFFER && path_size > 0,
            "读取安装路径失败（{}）",
            status.0
        );
        let mut path = vec![0u16; path_size as usize];
        let status = GetPackagePathByFullName(
            PCWSTR(names[0].0),
            &mut path_size,
            Some(PWSTR(path.as_mut_ptr())),
        );
        ensure!(status.0 == 0, "读取安装路径失败（{}）", status.0);
        let end = path.iter().position(|x| *x == 0).unwrap_or(path.len());
        let app_dir = PathBuf::from(std::ffi::OsString::from_wide(&path[..end])).join("app");
        ensure!(
            app_dir.join("ChatGPT.exe").is_file()
                && app_dir.join("resources").join("app.asar").is_file(),
            "安装包缺少应用文件"
        );
        Ok(Package {
            version: parse_version(&full_name)?,
            full_name,
            app_dir,
        })
    }
}

/// Verify both Authenticode trust and the leaf signer's exact common name.
pub fn verify_official(exe: &Path) -> Result<()> {
    let path = wide(exe.as_os_str());
    unsafe {
        let mut info = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: PCWSTR(path.as_ptr()),
            ..Default::default()
        };
        let mut data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
            dwStateAction: WTD_STATEACTION_VERIFY,
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        let result = (|| -> Result<()> {
            ensure!(
                status == 0,
                "源程序数字签名验证失败（0x{:08x}）",
                status as u32
            );
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            ensure!(!provider.is_null(), "无法读取签名者信息");
            let signer = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
            ensure!(!signer.is_null(), "无法读取签名者");
            let certificate = WTHelperGetProvCertFromChain(signer, 0);
            ensure!(
                !certificate.is_null() && !(*certificate).pCert.is_null(),
                "无法读取签名证书"
            );
            let cert = (*certificate).pCert;
            let len = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None);
            ensure!(len > 1, "签名者名称为空");
            let mut name = vec![0u16; len as usize];
            CertGetNameStringW(
                cert,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                None,
                Some(&mut name),
            );
            let name = String::from_utf16_lossy(&name[..name.len() - 1]);
            ensure!(name == "OpenAI OpCo, LLC", "签名者不匹配：{name}");
            Ok(())
        })();
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        result
    }
}

/// Create the user's desktop shortcut without invoking a shell.
pub fn create_shortcut(icon: Option<&Path>) -> Result<PathBuf> {
    unsafe {
        let desktop = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)?;
        let desktop_path = PathBuf::from(std::ffi::OsString::from_wide(desktop.as_wide()));
        CoTaskMemFree(Some(desktop.0.cast()));
        create_shortcut_in(&desktop_path, icon)
    }
}

fn create_shortcut_in(desktop_path: &Path, icon: Option<&Path>) -> Result<PathBuf> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .context("COM 初始化失败")?;
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }
        let _guard = ComGuard;
        let path = desktop_path.join("ChatGPT(i18n).lnk");
        ensure!(
            !path.exists(),
            "桌面已存在 ChatGPT(i18n) 快捷方式；请先重命名或移除已有文件"
        );
        let current_exe = std::env::current_exe()?;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(wide(current_exe.as_os_str()).as_ptr()))?;
        link.SetArguments(w!("--launch"))?;
        if let Some(parent) = current_exe.parent() {
            link.SetWorkingDirectory(PCWSTR(wide(parent.as_os_str()).as_ptr()))?;
        }
        if let Some(icon) = icon {
            link.SetIconLocation(PCWSTR(wide(icon.as_os_str()).as_ptr()), 0)?;
        }
        // Save in a unique directory on the same volume. MoveFileW refuses to
        // overwrite an existing target, including one created concurrently.
        let temp = tempfile::Builder::new()
            .prefix(".codex-i18n-shortcut-")
            .tempdir_in(desktop_path)?;
        let staged = temp.path().join("shortcut.lnk");
        let persist: IPersistFile = link.cast()?;
        persist.Save(PCWSTR(wide(staged.as_os_str()).as_ptr()), true)?;
        MoveFileW(
            PCWSTR(wide(staged.as_os_str()).as_ptr()),
            PCWSTR(wide(path.as_os_str()).as_ptr()),
        )
        .context("无法放置快捷方式；目标可能已存在")?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_lock_prevents_concurrent_builds() {
        let name = w!("Local\\CodexI18nTestBuildLock");
        let lock = named_build_lock(name).unwrap();
        assert!(
            std::thread::spawn(|| named_build_lock(w!("Local\\CodexI18nTestBuildLock")).is_err())
                .join()
                .unwrap()
        );
        drop(lock);
        assert!(named_build_lock(name).is_ok());
    }
    #[test]
    fn shortcut_points_to_launcher_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = create_shortcut_in(dir.path(), None).unwrap();
        assert_eq!(path.file_name().unwrap(), "ChatGPT(i18n).lnk");
        let original = std::fs::read(&path).unwrap();
        assert!(create_shortcut_in(dir.path(), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        unsafe {
            use windows::Win32::System::Com::STGM_READ;
            use windows::Win32::UI::Shell::SLGP_RAWPATH;
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            persist
                .Load(PCWSTR(wide(path.as_os_str()).as_ptr()), STGM_READ)
                .unwrap();
            let mut target = vec![0u16; 32768];
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .unwrap();
            let end = target.iter().position(|&c| c == 0).unwrap();
            assert_eq!(
                PathBuf::from(std::ffi::OsString::from_wide(&target[..end])),
                std::env::current_exe().unwrap()
            );
            let mut arguments = vec![0u16; 100];
            link.GetArguments(&mut arguments).unwrap();
            let end = arguments.iter().position(|&c| c == 0).unwrap();
            assert_eq!(String::from_utf16_lossy(&arguments[..end]), "--launch");
            drop(persist);
            drop(link);
            CoUninitialize();
        }
    }
    #[test]
    #[ignore = "requires a current-user OpenAI.Codex Store installation"]
    fn installed_package_and_signature() {
        let package = find_package().unwrap();
        eprintln!("package: {}", package.full_name);
        verify_official(&package.app_dir.join("ChatGPT.exe")).unwrap();
        let plan = crate::patch::inspect(
            &package.app_dir.join("resources").join("app.asar"),
            &package.app_dir.join("ChatGPT.exe"),
        )
        .unwrap();
        eprintln!("resource: {}", plan.resource);
    }
    #[test]
    fn versions_are_four_unsigned_parts() {
        assert_eq!(
            parse_version("OpenAI.Codex_26.1103.4239.0_x64__2p2nqsd0c76g0").unwrap(),
            [26, 1103, 4239, 0]
        );
        assert!(parse_version("OpenAI.Codex_1.2.3_x64__x").is_err());
    }
}
