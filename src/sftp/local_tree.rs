//! 本地上传目录扫描。
//!
//! [`scan_directory`] 只读一层，供上传管线边扫描边传；测试辅助扫描保留完整树快照，供
//! 单元测试和其它不需要“立即首字节”的调用方使用。
//!
//! 扫描是纯 `std::fs`，没有网络、没有 async —— 所以能直接单测。
//!
//! 三条规则：
//!
//! * **安全的相对符号链接原样记录**。链接不会被跟随；解析后仍位于上传根目录内的
//!   相对链接交给上传端复制，绝对链接和越界链接跳过。
//! * **隐藏文件照传**。面板上的「显示隐藏文件」只影响列表渲染；整目录上传
//!   如果偷偷漏掉 `.env`，那才是最坏的一种「安静」。
//! * **空目录也记下来**。远端该建的目录一个都不能少，否则 `rsync` 之类
//!   依赖目录结构的工具会莫名其妙地失败。

use std::path::{Path, PathBuf};

/// 一个待上传的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// 本地绝对路径。
    pub local: PathBuf,
    /// 相对根目录的路径（永远是相对形式，不含根）。
    pub relative: PathBuf,
    pub size: u64,
}

/// 一个待上传的符号链接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSymlink {
    /// 链接在上传根目录内的相对路径。
    pub relative: PathBuf,
    /// 原始相对目标；上传到 POSIX 服务器时保持同样的相对关系。
    pub target: PathBuf,
}

/// 一棵本地目录树的快照。
#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalTree {
    /// 子目录（相对根），**父目录一定排在子目录前面** —— 按它建目录不必再排序。
    pub dirs: Vec<PathBuf>,
    /// 文件，顺序按目录遍历顺序（可复现）。
    pub files: Vec<TreeFile>,
    /// 解析后不会逃出上传根目录的相对符号链接。
    pub symlinks: Vec<TreeSymlink>,
    /// 所有文件大小之和，用于整批进度。
    pub total_bytes: u64,
    /// 跳过的条目数：符号链接、socket/fifo、读不到权限的目录或文件。
    pub skipped: usize,
}

/// 一个目录的直接子项；不递归进入 `dirs`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalDirectory {
    pub dirs: Vec<PathBuf>,
    pub files: Vec<TreeFile>,
    pub symlinks: Vec<TreeSymlink>,
    pub total_bytes: u64,
    pub skipped: usize,
}

/// 扫描 `root` 这棵目录树。
///
/// `root` 必须是目录；根目录本身不出现在 `dirs` 里（调用方负责在远端建它）。
#[cfg(test)]
pub fn scan(root: &Path) -> Result<LocalTree, String> {
    let mut tree = LocalTree::default();
    // 显式栈而不是递归：几十层深的 node_modules 不该把栈打穿。
    let mut pending: Vec<PathBuf> = vec![PathBuf::new()];

    while let Some(relative) = pending.pop() {
        let directory = match scan_directory(root, &relative) {
            Ok(directory) => directory,
            Err(err) => {
                if relative.as_os_str().is_empty() {
                    return Err(err);
                }
                // 读不到就跳过这一整个目录，别让一个没权限的子目录毁掉整次上传。
                log::warn!("跳过目录 {}：{err}", root.join(&relative).display());
                tree.skipped += 1;
                continue;
            }
        };
        pending.extend(directory.dirs.iter().cloned());
        tree.dirs.extend(directory.dirs);
        tree.files.extend(directory.files);
        tree.symlinks.extend(directory.symlinks);
        tree.total_bytes += directory.total_bytes;
        tree.skipped += directory.skipped;
    }

    // 父目录必须排在子目录前面，否则远端 mkdir 会撞上「父目录不存在」。
    // `sort_by_key` 是稳定排序，同层的目录保持遍历顺序。
    tree.dirs.sort_by_key(|dir| dir.components().count());

    Ok(tree)
}

/// 扫描 `root/relative` 的直接子项，不递归。
pub fn scan_directory(root: &Path, relative: &Path) -> Result<LocalDirectory, String> {
    let dir = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|err| format!("读取本地目录 {} 失败：{err}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} 是符号链接，拒绝递归上传", dir.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} 不是目录", dir.display()));
    }

    let reader = std::fs::read_dir(&dir)
        .map_err(|err| format!("读取本地目录 {} 失败：{err}", dir.display()))?;
    let mut directory = LocalDirectory::default();
    let mut items = Vec::new();
    for item in reader {
        match item {
            Ok(item) => items.push(item),
            Err(err) => {
                log::warn!("读取目录项失败：{err}");
                directory.skipped += 1;
            }
        }
    }
    items.sort_by_key(|item| item.file_name());

    for item in items {
        let child_relative = relative.join(item.file_name());
        let path = item.path();
        let Ok(file_type) = item.file_type() else {
            directory.skipped += 1;
            continue;
        };

        if file_type.is_symlink() {
            match std::fs::read_link(&path) {
                Ok(target) if safe_relative_link(&child_relative, &target) => {
                    directory.symlinks.push(TreeSymlink {
                        relative: child_relative,
                        target,
                    });
                }
                Ok(_) | Err(_) => directory.skipped += 1,
            }
        } else if file_type.is_dir() {
            directory.dirs.push(child_relative);
        } else if file_type.is_file() {
            let size = item.metadata().map(|meta| meta.len()).unwrap_or(0);
            directory.total_bytes += size;
            directory.files.push(TreeFile {
                local: path,
                relative: child_relative,
                size,
            });
        } else {
            directory.skipped += 1;
        }
    }

    Ok(directory)
}

/// 判断相对链接按「链接所在目录 + target」解析后是否仍在上传根目录内。
///
/// 不调用 `canonicalize`：pnpm 的链接可能指向稍后才扫描到的条目，而且我们只需要
/// 防止 `../../..` 逃出根目录，并不应该跟随链接。
fn safe_relative_link(link: &Path, target: &Path) -> bool {
    use std::path::Component;

    if target.is_absolute() {
        return false;
    }

    let mut depth = link
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// 相对路径 → POSIX 形式的字符串（`sub/deep`）。
///
/// 显式用 `/` 拼接，避免 Windows 分隔符混入远程 POSIX 路径。
pub fn relative_to_posix(relative: &Path) -> String {
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立的临时目录，避免并行时互相踩。
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("macscp-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建父目录");
        }
        std::fs::write(path, data).expect("写文件");
    }

    #[test]
    fn scan_collects_dirs_and_files_with_sizes() {
        let root = scratch("basic");
        write(&root.join("a.txt"), b"hello");
        write(&root.join("sub/b.txt"), b"world!");
        write(&root.join("sub/deep/c.bin"), &[0u8; 10]);
        std::fs::create_dir_all(root.join("empty")).expect("建空目录");

        let tree = scan(&root).expect("扫描应成功");

        assert_eq!(tree.files.len(), 3, "三个文件都要在清单里");
        assert_eq!(tree.total_bytes, 5 + 6 + 10);
        assert_eq!(tree.skipped, 0);

        // 空目录不能漏 —— 远端该建的目录一个都不能少。
        let dirs: Vec<String> = tree.dirs.iter().map(|dir| relative_to_posix(dir)).collect();
        assert!(dirs.contains(&"empty".to_string()));
        assert!(dirs.contains(&"sub".to_string()));
        assert!(dirs.contains(&"sub/deep".to_string()));

        // 父目录必须排在子目录前面，否则 mkdir 会撞「父目录不存在」。
        let sub = dirs.iter().position(|dir| dir == "sub").expect("sub");
        let deep = dirs.iter().position(|dir| dir == "sub/deep").expect("deep");
        assert!(sub < deep, "父目录应排在子目录前面：{dirs:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 根目录本身不进 `dirs`：远端那个目录由调用方按目标名创建。
    #[test]
    fn scan_does_not_include_the_root_itself() {
        let root = scratch("root");
        write(&root.join("only.txt"), b"x");

        let tree = scan(&root).expect("扫描应成功");
        assert!(tree.dirs.is_empty());
        assert_eq!(tree.files.len(), 1);
        assert_eq!(relative_to_posix(&tree.files[0].relative), "only.txt");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 安全的相对链接只记录、不跟随；绝对链接与越界链接仍跳过。
    #[cfg(unix)]
    #[test]
    fn scan_keeps_safe_relative_symlinks_and_skips_unsafe_ones() {
        let root = scratch("symlink");
        write(&root.join("real.txt"), b"data");
        std::fs::create_dir_all(root.join("bin")).expect("建链接目录");
        std::os::unix::fs::symlink("../real.txt", root.join("bin/link.txt"))
            .expect("建安全相对链接");
        std::os::unix::fs::symlink("../../outside", root.join("bin/escape")).expect("建越界链接");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("建环形链接");

        let tree = scan(&root).expect("扫描应成功");

        assert_eq!(tree.files.len(), 1, "只应留下真实文件");
        assert_eq!(tree.files[0].relative, PathBuf::from("real.txt"));
        assert_eq!(
            tree.symlinks,
            vec![TreeSymlink {
                relative: PathBuf::from("bin/link.txt"),
                target: PathBuf::from("../real.txt"),
            }]
        );
        assert_eq!(tree.skipped, 2, "越界链接和绝对环形链接都算跳过");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_a_symlink_root() {
        let parent = scratch("symlink-root");
        let real = parent.join("real");
        std::fs::create_dir(&real).expect("建真实目录");
        let link = parent.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("建根目录链接");

        let error = scan(&link).expect_err("根链接不能被递归上传");
        assert!(error.contains("符号链接"));

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// 隐藏文件照传：整目录上传悄悄漏掉 `.env` 是最坏的安静。
    #[test]
    fn scan_includes_hidden_files() {
        let root = scratch("hidden");
        write(&root.join(".env"), b"SECRET=1");
        write(&root.join("app.rs"), b"fn main() {}");

        let tree = scan(&root).expect("扫描应成功");
        let names: Vec<String> = tree
            .files
            .iter()
            .map(|file| relative_to_posix(&file.relative))
            .collect();
        assert!(names.contains(&".env".to_string()), "隐藏文件不能被过滤");
        assert!(names.contains(&"app.rs".to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_rejects_a_plain_file() {
        let root = scratch("notdir");
        let file = root.join("a.txt");
        write(&file, b"x");

        let err = scan(&file).expect_err("文件不是目录");
        assert!(err.contains("不是目录"), "错误信息应说清原因：{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_paths_are_posix_joined() {
        assert_eq!(
            relative_to_posix(Path::new("sub/deep/file.txt")),
            "sub/deep/file.txt"
        );
        assert_eq!(relative_to_posix(Path::new("file.txt")), "file.txt");
    }
}
