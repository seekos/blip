pub fn join(parent: &str, name: &str) -> String {
    let parent = if parent.is_empty() { "/" } else { parent };
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

pub fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        return None;
    }
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    Some(if parent.is_empty() { "/" } else { parent }.to_string())
}

#[cfg(test)]
mod tests {
    use super::{join, parent};

    #[test]
    fn keeps_remote_paths_absolute_and_walks_to_root() {
        assert_eq!(join("/", "tmp"), "/tmp");
        assert_eq!(join("/tmp/", "file.txt"), "/tmp/file.txt");
        assert_eq!(parent("/tmp/file.txt").as_deref(), Some("/tmp"));
        assert_eq!(parent("/tmp").as_deref(), Some("/"));
        assert_eq!(parent("/"), None);
    }
}
