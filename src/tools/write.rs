//! `write`: replace a whole file.

use crate::files;
use crate::protocol::Request;

pub async fn write(request: &Request) -> Result<String, String> {
    let Some(path) = request.arg("path").filter(|p| !p.is_empty()) else {
        return Err("write requires path".into());
    };
    let Some(content) = request.arg("content") else {
        // An empty string is a real request to truncate; an absent argument is the
        // model forgetting the parameter, and the two must not do the same thing.
        return Err("write requires content".into());
    };
    files::replace(path, content.as_bytes().to_vec(), None).await?;
    Ok(format!("Wrote {} bytes to {path}\n", content.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_request;

    fn request(args: &str) -> crate::protocol::Request {
        parse_request(format!(r#"{{"id":1,"tool":"write","args":{{{args}}}}}"#).as_bytes()).unwrap()
    }

    /// A file path inside a fresh directory that deletes itself with the test.  The
    /// caller holds the directory by keeping the second half of the pair, which is
    /// what the underscore in front of its name means.
    fn temp(tag: &str) -> (String, tempfile::TempDir) {
        let dir = tempfile::TempDir::with_prefix(format!("ds4-helper-write-{tag}-")).unwrap();
        (dir.path().join("file").to_str().unwrap().to_string(), dir)
    }

    #[tokio::test]
    async fn a_write_reports_the_byte_count_not_the_character_count() {
        let (path, _dir) = temp("bytes");
        let text = write(&request(&format!(
            r#""path":"{path}","content":"héllo 中""#
        )))
        .await
        .unwrap();
        // h, 2 bytes for the e-acute, llo, a space, 3 bytes for the ideograph.
        assert!(text.starts_with("Wrote 10 bytes to "), "{text}");
        assert_eq!(std::fs::read(&path).unwrap(), "héllo 中".as_bytes());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn empty_content_truncates_and_absent_content_is_an_error() {
        let (path, _dir) = temp("truncate");
        std::fs::write(&path, "previous").unwrap();
        write(&request(&format!(r#""path":"{path}","content":"""#)))
            .await
            .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().is_empty());
        std::fs::remove_file(path).unwrap();

        let err = write(&request(r#""path":"/tmp/x""#)).await.unwrap_err();
        assert_eq!(err, "write requires content");
    }

    #[tokio::test]
    async fn a_missing_directory_is_reported_not_panicked() {
        let err = write(&request(r#""path":"/no/such/dir/file","content":"x""#))
            .await
            .unwrap_err();
        assert!(!err.is_empty());
        let err = write(&request(r#""content":"x""#)).await.unwrap_err();
        assert_eq!(err, "write requires path");
    }
}
