//! 下载文件夹之前，先把远端目录树扫成一份「待办清单」。
//!
//! 和 [`super::local_tree`] 是一对镜像：那边扫本地磁盘（同步 `std::fs`），
//! 这边扫远端（异步 SFTP `read_dir`）。分开写是因为两边的错误模型不同 ——
//! 本地读不到目录只是权限问题，远端读不到可能是连接断了，得让上层看见。
//!
//! 规则和上传保持一致：
//!
//! * **符号链接跳过**（不跟随、也不在本地建链接），个数记在 [`RemoteTree::skipped`]；
//! * **空目录也记下来** —— 本地该建的目录一个都不能少；
//! * 父目录一定排在子目录前面（按路径深度排），建目录不必再排序。

use std::path::PathBuf;

use russh_sftp::client::SftpSession;

use super::session;
use super::types::{EntryKind, FileEntry};

/// 一个待下载的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    /// 远端绝对路径。
    pub remote: String,
    /// 相对根目录的路径（用来在本地拼目标路径）。
    pub relative: PathBuf,
    pub size: u64,
}

/// 一棵远端目录树的快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteTree {
    /// 子目录（相对根），**父目录一定排在子目录前面**。
    pub dirs: Vec<PathBuf>,
    pub files: Vec<RemoteFile>,
    pub total_bytes: u64,
    /// 跳过的条目数：符号链接、读不到的子目录。
    pub skipped: usize,
}

/// 递归扫描 `root` 下面的整棵树。
///
/// 根目录读不出来直接返回错误（多半是路径选错了）；某个子目录读不出来只跳过并计数，
/// 一个没权限的子目录不该让整次下载失败。
pub async fn scan(sftp: &SftpSession, root: &str) -> Result<RemoteTree, String> {
    let mut tree = RemoteTree::default();

    // 根目录先单独读一次：它的失败是「目标不对」，必须报上去。
    let entries = session::list_dir(sftp, root).await?;
    let mut pending: Vec<Pending> = Vec::new();
    absorb(&mut tree, &mut pending, PathBuf::new(), 0, entries);

    while let Some((dir, relative, depth)) = pending.pop() {
        match session::list_dir(sftp, &dir).await {
            Ok(entries) => absorb(&mut tree, &mut pending, relative, depth, entries),
            Err(err) => {
                log::warn!("跳过远端目录 {dir}：{err}");
                tree.skipped += 1;
            }
        }
    }

    // 父目录必须排在子目录前面，否则本地 mkdir 会撞上「父目录不存在」。
    tree.dirs.sort_by_key(|dir| dir.components().count());

    Ok(tree)
}

/// 待扫的子目录：远端路径、相对路径、深度。
type Pending = (String, PathBuf, usize);

/// 递归深度上限。
///
/// 正常情况下这棵树最多几十层。设上限是为了兜住一种坏情况：某些服务器在 `readdir`
/// 里把「指向目录的符号链接」报成目录（跟随后再 stat 的结果），于是 `a -> ..` 这样的
/// 链接会让扫描无限往下走。`local_tree` 那边有 `symlink_metadata` 兜着，远端只能靠
/// 深度封顶 —— 到顶就当作跳过，宁可少传一层也不能挂死。
const MAX_DEPTH: usize = 64;

/// 把一层目录项收进树里：目录入待办栈，文件直接记账，符号链接跳过。
fn absorb(
    tree: &mut RemoteTree,
    pending: &mut Vec<Pending>,
    relative: PathBuf,
    depth: usize,
    entries: Vec<FileEntry>,
) {
    for entry in entries {
        if !session::is_safe_entry_name(&entry.name) {
            log::warn!("跳过服务器返回的危险文件名：{:?}", entry.name);
            tree.skipped += 1;
            continue;
        }
        let child = relative.join(&entry.name);
        match entry.kind {
            EntryKind::Directory => {
                tree.dirs.push(child.clone());
                if depth + 1 >= MAX_DEPTH {
                    log::warn!("目录太深，跳过 {}（{MAX_DEPTH} 层封顶）", entry.path);
                    tree.skipped += 1;
                    continue;
                }
                pending.push((entry.path, child, depth + 1));
            }
            EntryKind::File => {
                tree.total_bytes += entry.size;
                tree.files.push(RemoteFile {
                    remote: entry.path,
                    relative: child,
                    size: entry.size,
                });
            }
            // 符号链接不跟：可能指向根目录外面，也可能绕成环。
            EntryKind::Symlink => tree.skipped += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: format!("/home/tester/root/{name}"),
            kind,
            hidden: name.starts_with('.'),
            size,
            modified: None,
            mode: None,
        }
    }

    /// 一层目录项的分类：目录入栈、文件记账、链接跳过。
    #[test]
    fn absorb_sorts_entries_into_dirs_files_and_skips() {
        let mut tree = RemoteTree::default();
        let mut pending = Vec::new();

        absorb(
            &mut tree,
            &mut pending,
            PathBuf::new(),
            0,
            vec![
                entry("sub", EntryKind::Directory, 0),
                entry("a.txt", EntryKind::File, 10),
                entry("link", EntryKind::Symlink, 0),
            ],
        );

        assert_eq!(tree.dirs, vec![PathBuf::from("sub")]);
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.files[0].relative, PathBuf::from("a.txt"));
        assert_eq!(tree.total_bytes, 10);
        assert_eq!(tree.skipped, 1);
        assert_eq!(pending.len(), 1, "目录要进待办栈");
        assert_eq!(pending[0].1, PathBuf::from("sub"));
        assert_eq!(pending[0].2, 1, "子目录的深度是父目录 +1");
    }

    /// 相对路径是逐层拼的，不是覆盖。
    #[test]
    fn absorb_keeps_the_relative_prefix() {
        let mut tree = RemoteTree::default();
        let mut pending = Vec::new();

        absorb(
            &mut tree,
            &mut pending,
            PathBuf::from("sub"),
            1,
            vec![entry("c.bin", EntryKind::File, 5)],
        );

        assert_eq!(tree.files[0].relative, PathBuf::from("sub/c.bin"));
    }

    #[test]
    fn absorb_rejects_server_names_that_escape_the_root() {
        let mut tree = RemoteTree::default();
        let mut pending = Vec::new();

        absorb(
            &mut tree,
            &mut pending,
            PathBuf::new(),
            0,
            vec![
                entry("../owned", EntryKind::File, 5),
                entry("/absolute", EntryKind::Directory, 0),
                entry("safe.txt", EntryKind::File, 3),
            ],
        );

        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.files[0].relative, PathBuf::from("safe.txt"));
        assert!(pending.is_empty());
        assert_eq!(tree.skipped, 2);
    }

    /// 深度到顶的目录只记「跳过」，不再入栈 —— 这是「服务器把目录链接报成目录」
    /// 时唯一的刹车，没有它扫描会一直往下走。
    #[test]
    fn absorb_stops_at_the_depth_cap() {
        let mut tree = RemoteTree::default();
        let mut pending = Vec::new();

        absorb(
            &mut tree,
            &mut pending,
            PathBuf::from("deep"),
            MAX_DEPTH - 1,
            vec![entry("loop", EntryKind::Directory, 0)],
        );

        assert!(pending.is_empty(), "到顶就不该再往下扫");
        assert_eq!(tree.skipped, 1);
        // 目录本身仍然要建出来：本地结构和远端能对上的那部分是完整的。
        assert_eq!(tree.dirs, vec![PathBuf::from("deep/loop")]);
    }
}
