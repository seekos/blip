//! 传输和文件操作共用的路径互斥器。
//!
//! SFTP 没有事务；两个任务同时写同一个 `.filepart`，或删除父目录时另一个任务
//! 正在上传子文件，都会造成不可预测的结果。这里按命名空间（本地/远端）和路径层级
//! 加锁：同一路径、父路径和子路径互相冲突，不相关目录仍可并发。

use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Default)]
pub struct PathLocks {
    held: Mutex<BTreeSet<String>>,
    changed: Notify,
}

pub struct PathGuard {
    locks: Arc<PathLocks>,
    keys: Vec<String>,
}

impl PathLocks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn acquire(self: &Arc<Self>, keys: impl IntoIterator<Item = String>) -> PathGuard {
        let mut keys: Vec<String> = keys.into_iter().collect();
        keys.sort();
        keys.dedup();

        loop {
            let notified = self.changed.notified();
            let acquired = {
                let mut held = self
                    .held
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if keys
                    .iter()
                    .all(|wanted| held.iter().all(|current| !paths_conflict(wanted, current)))
                {
                    held.extend(keys.iter().cloned());
                    true
                } else {
                    false
                }
            };
            if acquired {
                return PathGuard {
                    locks: self.clone(),
                    keys,
                };
            }
            notified.await;
        }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        let mut held = self
            .locks
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in &self.keys {
            held.remove(key);
        }
        drop(held);
        self.locks.changed.notify_waiters();
    }
}

pub fn remote_key(path: &str) -> String {
    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(part),
        }
    }
    if components.is_empty() {
        "remote:/".to_string()
    } else {
        format!("remote:/{}", components.join("/"))
    }
}

pub fn local_key(path: &Path) -> String {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(part) => components.push(part.to_string_lossy().into_owned()),
            Component::Prefix(prefix) => {
                components.push(prefix.as_os_str().to_string_lossy().into_owned())
            }
        }
    }
    if components.is_empty() {
        "local:/".to_string()
    } else {
        format!("local:/{}", components.join("/"))
    }
}

fn paths_conflict(left: &str, right: &str) -> bool {
    let Some((left_space, left_path)) = left.split_once(':') else {
        return left == right;
    };
    let Some((right_space, right_path)) = right.split_once(':') else {
        return left == right;
    };
    if left_space != right_space {
        return false;
    }
    is_same_or_ancestor(left_path, right_path) || is_same_or_ancestor(right_path, left_path)
}

fn is_same_or_ancestor(parent: &str, child: &str) -> bool {
    parent == child
        || parent == "/"
        || child
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_same_namespace_ancestors_conflict() {
        assert!(paths_conflict("remote:/srv/app", "remote:/srv/app/a"));
        assert!(paths_conflict("local:/tmp/a", "local:/tmp/a"));
        assert!(!paths_conflict("remote:/srv/app", "remote:/srv/apple"));
        assert!(!paths_conflict("remote:/tmp/a", "local:/tmp/a"));
        assert_eq!(remote_key("/srv/app/../release"), "remote:/srv/release");
        assert_eq!(remote_key("/../../"), "remote:/");
    }

    #[tokio::test]
    async fn guard_releases_the_path_for_the_next_task() {
        let locks = PathLocks::new();
        let first = locks.acquire([remote_key("/srv/app")]).await;
        let waiting = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _guard = locks.acquire([remote_key("/srv/app/file")]).await;
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("释放父路径后子路径锁应可取得")
            .expect("等待任务不应 panic");
    }
}
