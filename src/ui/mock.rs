//! 骨架阶段的假数据。
//!
//! 目的很具体：在没有服务器、也不想读真实磁盘的情况下，把双栏布局、图标区分、
//! 列宽与排序都摊开看清楚。真实数据一旦到达（刷新本地 / 连接成功），
//! `FilePane::set_entries` 会覆盖这里的条目。

use std::time::{Duration, SystemTime};

use crate::sftp::session::sort_entries;
use crate::sftp::types::{EntryKind, FileEntry};

/// 左侧「本地」面板的假目录。
pub fn local_entries(root: &str) -> Vec<FileEntry> {
    let mut entries = vec![
        entry(root, "Desktop", EntryKind::Directory, 0, 0o040_755, 3_600),
        entry(root, "Documents", EntryKind::Directory, 0, 0o040_755, 86_400),
        entry(root, "Downloads", EntryKind::Directory, 0, 0o040_755, 900),
        entry(root, "Pictures", EntryKind::Directory, 0, 0o040_755, 172_800),
        entry(root, "Projects", EntryKind::Directory, 0, 0o040_755, 5_400),
        entry(root, ".ssh", EntryKind::Directory, 0, 0o040_700, 604_800),
        entry(root, ".zshrc", EntryKind::File, 3_204, 0o100_600, 259_200),
        entry(root, "archive.zip", EntryKind::File, 24_117_248, 0o100_644, 43_200),
        entry(root, "budget-2026.xlsx", EntryKind::File, 88_320, 0o100_644, 7_200),
        entry(root, "notes.md", EntryKind::File, 12_480, 0o100_644, 1_800),
        entry(root, "screenshot.png", EntryKind::File, 1_842_176, 0o100_644, 600),
        entry(root, "setup.sh", EntryKind::File, 1_048, 0o100_755, 86_400),
    ];
    sort_entries(&mut entries);
    entries
}

/// 右侧「远程」面板的假目录。
pub fn remote_entries(root: &str) -> Vec<FileEntry> {
    let mut entries = vec![
        entry(root, "deploy", EntryKind::Directory, 0, 0o040_755, 1_200),
        entry(root, "logs", EntryKind::Directory, 0, 0o040_750, 300),
        entry(root, "public", EntryKind::Directory, 0, 0o040_755, 604_800),
        entry(root, "src", EntryKind::Directory, 0, 0o040_755, 600),
        entry(root, ".env", EntryKind::File, 512, 0o100_600, 604_800),
        entry(root, "app.log", EntryKind::File, 12_884_901, 0o100_640, 60),
        entry(root, "Cargo.lock", EntryKind::File, 48_112, 0o100_644, 3_600),
        entry(root, "Cargo.toml", EntryKind::File, 1_204, 0o100_644, 3_600),
        entry(root, "docker-compose.yml", EntryKind::File, 2_088, 0o100_644, 86_400),
        entry(root, "README.md", EntryKind::File, 6_442, 0o100_644, 7_200),
        entry(root, "release.tar.gz", EntryKind::File, 84_213_760, 0o100_644, 1_800),
    ];
    sort_entries(&mut entries);
    entries
}

fn entry(
    root: &str,
    name: &str,
    kind: EntryKind,
    size: u64,
    mode: u32,
    ago_secs: u64,
) -> FileEntry {
    FileEntry {
        name: name.to_string(),
        path: join(root, name),
        kind,
        size,
        modified: Some(SystemTime::now() - Duration::from_secs(ago_secs)),
        mode: Some(mode),
    }
}

fn join(root: &str, name: &str) -> String {
    if root.ends_with('/') {
        format!("{root}{name}")
    } else {
        format!("{root}/{name}")
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` 会把 `gpui_kit::*` 里的 `gpui_macros::test` 一起带进来，
    // 盖掉标准库的 `#[test]`。显式导入真正的 `test` 宏压过 glob。
    use ::core::prelude::v1::test;

    use super::*;

    #[test]
    fn mock_entries_are_sorted_and_have_paths() {
        let entries = remote_entries("/srv/app");
        assert!(entries.len() >= 8);
        // 目录在前
        assert!(entries[0].is_dir());
        // 路径拼接正确
        assert!(entries.iter().all(|e| e.path.starts_with("/srv/app/")));
    }
}
