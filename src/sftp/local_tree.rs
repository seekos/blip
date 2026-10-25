//! 上传文件夹之前，先把本地目录树扫成一份「待办清单」。
//!
//! 为什么不在上传过程中边走边传：目录里可能有几千个文件，一边 `read_dir`
//! 一边上传会让进度条永远算不出总数（`total` 只能是 0），用户看不到「还剩多少」。
//! 先扫一遍拿到全部大小，`TransferStarted` 才能给出一个诚实的百分比。
//!
//! 扫描是纯 `std::fs`，没有网络、没有 async —— 所以能直接单测。
//!
//! 三条规则：
//!
//! * **符号链接一律跳过**。跟随会绕圈（`a -> ..`），也会把根目录之外的东西
//!   悄悄传上服务器。跳过数量记在 [`LocalTree::skipped`] 里，由调用方提示。
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

/// 一棵本地目录树的快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalTree {
    /// 子目录（相对根），**父目录一定排在子目录前面** —— 按它建目录不必再排序。
    pub dirs: Vec<PathBuf>,
    /// 文件，顺序按目录遍历顺序（可复现）。
    pub files: Vec<TreeFile>,
    /// 所有文件大小之和，用于整批进度。
    pub total_bytes: u64,
    /// 跳过的条目数：符号链接、socket/fifo、读不到权限的目录或文件。
    pub skipped: usize,
}

/// 扫描 `root` 这棵目录树。
///
/// `root` 必须是目录；根目录本身不出现在 `dirs` 里（调用方负责在远端建它）。
pub fn scan(root: &Path) -> Result<LocalTree, String> {
    let meta = std::fs::metadata(root)
        .map_err(|err| format!("读取本地目录 {} 失败：{err}", root.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} 不是目录", root.display()));
    }

    let mut tree = LocalTree::default();
    // 显式栈而不是递归：几十层深的 node_modules 不该把栈打穿。
    let mut pending: Vec<(PathBuf, PathBuf)> = vec![(root.to_path_buf(), PathBuf::new())];

    while let Some((dir, relative)) = pending.pop() {
        let reader = match std::fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(err) => {
                // 读不到就跳过这一整个目录，别让一个没权限的子目录毁掉整次上传。
                log::warn!("跳过目录 {}：{err}", dir.display());
                tree.skipped += 1;
                continue;
            }
        };

        // `read_dir` 的顺序依赖文件系统，排序之后同样的输入总是得到同样的清单
        // （测试可以断言顺序，日志也好读）。
        let mut items: Vec<_> = reader.flatten().collect();
        items.sort_by_key(|item| item.file_name());

        for item in items {
            let child_relative = relative.join(item.file_name());
            let path = item.path();

            // 用 `file_type()` 而不是 `metadata()`：前者是 lstat 语义，
            // 不会跟着符号链接跳进目标里。
            let Ok(file_type) = item.file_type() else {
                tree.skipped += 1;
                continue;
            };

            if file_type.is_symlink() {
                tree.skipped += 1;
                continue;
            }

            if file_type.is_dir() {
                tree.dirs.push(child_relative.clone());
                pending.push((path, child_relative));
            } else if file_type.is_file() {
                let size = item.metadata().map(|meta| meta.len()).unwrap_or(0);
                tree.total_bytes += size;
                tree.files.push(TreeFile {
                    local: path,
                    relative: child_relative,
                    size,
                });
            } else {
                // socket / fifo / 设备文件：没有「上传」的意义。
                tree.skipped += 1;
            }
        }
    }

    // 父目录必须排在子目录前面，否则远端 mkdir 会撞上「父目录不存在」。
    // `sort_by_key` 是稳定排序，同层的目录保持遍历顺序。
    tree.dirs.sort_by_key(|dir| dir.components().count());

    Ok(tree)
}

/// 相对路径 → POSIX 形式的字符串（`sub/deep`）。
///
/// macOS 的分隔符本来就是 `/`，但这里显式拼一遍，免得将来换平台时
/// 把 `\` 带进远端路径里。
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

    /// 符号链接跳过：跟着跳会绕圈，也可能把根目录外的文件传上去。
    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks() {
        let root = scratch("symlink");
        write(&root.join("real.txt"), b"data");
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt"))
            .expect("建符号链接");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("建环形链接");

        let tree = scan(&root).expect("扫描应成功");

        assert_eq!(tree.files.len(), 1, "只应留下真实文件");
        assert_eq!(tree.files[0].relative, PathBuf::from("real.txt"));
        assert_eq!(tree.skipped, 2, "链接和环形链接都算跳过");

        let _ = std::fs::remove_dir_all(&root);
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
