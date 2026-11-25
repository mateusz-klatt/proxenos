//! Expand the forwarding argument of familiar batch launchers. Never evaluate batch syntax.
//! Wrappers with several possible destinations or computed target paths cannot be followed.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Executable, Failure, FailureKind, absolute_env};

pub(super) fn locate(
    cli: &str,
    directories: &[PathBuf],
    path_dirs: usize,
) -> Option<Result<Executable, Failure>> {
    let file = directories
        .iter()
        .flat_map(|dir| ["exe", "cmd", "bat"].map(|ext| dir.join(cli).with_extension(ext)))
        .find(|path| path.is_file())?;
    Some(if file.extension() == Some("exe".as_ref()) {
        Ok(Executable {
            program: file,
            prefix: Vec::new(),
        })
    } else {
        expand(&file, &directories[..path_dirs])
    })
}

fn expand(shim: &Path, search: &[PathBuf]) -> Result<Executable, Failure> {
    let unsupported = || {
        Failure::new(
            FailureKind::UnsupportedLauncher,
            format!("Cannot follow batch launcher {}.", shim.display()),
        )
    };
    let source = fs::read_to_string(shim).map_err(|_| unsupported())?;
    let dir = shim.parent().expect("shim in a directory");
    // npm uses both %~dp0 directly and a variable assigned to it. Vendors choose different
    // variable names. No other environment or command substitutions are supported.
    let mut bases = vec![String::from("%~dp0"), String::from("%dp0%")];
    let mut npm_node = false;
    let mut invocation = None;
    for line in source.lines() {
        let line = line.trim().trim_start_matches('@');
        let lower = line.to_ascii_lowercase();
        if let Some(assignment) = lower.strip_prefix("set ") {
            let assignment = assignment.trim_matches('"');
            if let Some(value) = assignment.strip_prefix("_prog=") {
                if !matches!(
                    value,
                    "node" | "node.exe" | "%dp0%\\node.exe" | "%~dp0\\node.exe" | "%~dp0node.exe"
                ) {
                    return Err(unsupported());
                }
                npm_node = true;
            }
            if let Some(name) = assignment.strip_suffix("=%~dp0") {
                bases.push(format!("%{name}%"));
            }
        } else if let Some(call) = line.strip_suffix("%*")
            && !lower.starts_with("rem ")
            && invocation.replace(call.trim_end()).is_some()
        {
            return Err(unsupported());
        }
    }
    let call = invocation.ok_or_else(unsupported)?;
    // npm appends its invocation to this fixed batch epilogue. Other command chains are
    // opaque: executing their last operand could select a program the wrapper never calls.
    const NPM_EPILOGUE: &str = "endlocal & goto #_undefined_# 2>nul || title %comspec% &";
    let call = if call.to_ascii_lowercase().starts_with(NPM_EPILOGUE) {
        &call[NPM_EPILOGUE.len()..]
    } else {
        call
    };
    let words = arguments(call).ok_or_else(unsupported)?;
    let (literal, command) = words.split_last().ok_or_else(unsupported)?;
    let lower = literal.to_ascii_lowercase();
    let (base, rest) = match bases.iter().find(|base| lower.starts_with(*base)) {
        Some(base) => (dir, literal[base.len()..].trim_start_matches(['/', '\\'])),
        None => (Path::new(""), *literal),
    };
    let target = base.join(rest);
    if !target.is_absolute() || !target.is_file() || rest.contains(['!', '%']) {
        return Err(unsupported());
    }
    let extension = target
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let command: Vec<_> = command
        .iter()
        .map(|word| word.to_ascii_lowercase())
        .collect();
    let command: Vec<_> = command.iter().map(String::as_str).collect();
    let runtime_name = command.first().map(|runtime| {
        let path = bases
            .iter()
            .find_map(|base| runtime.strip_prefix(base))
            .unwrap_or(runtime);
        path.rsplit(['/', '\\']).next().unwrap()
    });
    let (program, prefix) = match command.as_slice() {
        [] | ["call"] if extension == "exe" => (target, Vec::new()),
        [_] if ["js", "cjs", "mjs"].contains(&extension.as_str())
            && (matches!(runtime_name, Some("node" | "node.exe"))
                || (runtime_name == Some("%_prog%") && npm_node)) =>
        {
            (node(dir, search)?, vec![target.into_os_string()])
        }
        [_, flags @ ..]
            if extension == "ps1"
                && matches!(runtime_name, Some("powershell" | "powershell.exe"))
                && matches!(
                    flags,
                    ["-file"] | ["-noprofile", "-executionpolicy", "bypass", "-file"]
                ) =>
        {
            let program = absolute_env("SystemRoot")
                .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|path| path.is_file())
                .ok_or_else(unsupported)?;
            // Preserve the shim's PowerShell policy flags exactly as supplied.
            let mut prefix: Vec<_> = words[1..words.len() - 1]
                .iter()
                .map(OsString::from)
                .collect();
            prefix.push(target.into_os_string());
            (program, prefix)
        }
        _ => return Err(unsupported()),
    };
    Ok(Executable { program, prefix })
}

// A literal argv without concatenation, escapes, redirections or control operators.
fn arguments(mut line: &str) -> Option<Vec<&str>> {
    let mut words = Vec::new();
    line = line.trim_start();
    while !line.is_empty() {
        let (word, rest) = match line.chars().next()? {
            quote @ ('"' | '\'') => line[1..].split_once(quote)?,
            _ => {
                let part = line.split_at(line.find(char::is_whitespace).unwrap_or(line.len()));
                if part.0.contains(['^', '&', '|', '<', '>', '(', ')']) {
                    return None;
                }
                part
            }
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        words.push(word);
        line = rest.trim_start();
    }
    Some(words)
}

fn node(shim_dir: &Path, search: &[PathBuf]) -> Result<PathBuf, Failure> {
    let missing = || {
        Failure::new(
            FailureKind::UnsupportedLauncher,
            "No native Node executable for the JavaScript launcher. Set PROXENOS_NODE_BIN to its absolute .exe path.",
        )
    };
    if std::env::var_os("PROXENOS_NODE_BIN").is_some() {
        return absolute_env("PROXENOS_NODE_BIN")
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            })
            .ok_or_else(missing);
    }
    std::iter::once(shim_dir.join("node.exe"))
        .chain(search.iter().map(|dir| dir.join("node.exe")))
        .chain(absolute_env("LOCALAPPDATA").map(|root| root.join("Programs/nodejs/node.exe")))
        .chain(absolute_env("ProgramFiles").map(|root| root.join("nodejs/node.exe")))
        .find(|path| path.is_file())
        .ok_or_else(missing)
}
