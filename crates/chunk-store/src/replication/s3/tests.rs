use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
};

use super::*;

/// An S3 endpoint that refuses every request's signature as S3 does, echoing the canonical request with its session
/// token. Records each request's session token.
fn refusing() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let tokens = Arc::new(Mutex::new(Vec::new()));
    let recorded = tokens.clone();
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let mut connection = connection.unwrap();
            let mut reader = BufReader::new(&connection);
            let (mut token, mut length) = (None, 0);
            reader.read_line(&mut String::new()).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let Some((name, value)) = line.trim_end().split_once(':') else { break };
                match name.to_ascii_lowercase().as_str() {
                    "x-amz-security-token" => token = Some(value.trim().to_owned()),
                    "content-length" => length = value.trim().parse().unwrap(),
                    _ => {}
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>SignatureDoesNotMatch</Code><Message>The \
                 request signature we calculated does not match the signature you provided.</Message>\
                 <CanonicalRequest>PUT\n/bucket/log/segment\n\nx-amz-security-token:{}\n</CanonicalRequest></Error>",
                token.as_deref().unwrap_or_default()
            );
            recorded.lock().unwrap().push(token);
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: \
                 close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(response.as_bytes()).unwrap();
        }
    });
    (endpoint, tokens)
}

#[test]
fn requests_carry_the_current_session_token_and_errors_drop_what_s3_echoes() {
    let (endpoint, tokens) = refusing();
    let bucket = S3Bucket { name: "bucket".into(), region: "us-east-1".into(), endpoint: Some(endpoint), prefix: None };
    let temporary = |token: &str| S3Credentials::new("key".into(), "secret".into(), Some(token.into()));
    let credentials = temporary("first-token");
    let s3 = S3::new(&bucket, credentials.clone()).unwrap();

    let error = s3.put("log/segment", b"commits".to_vec()).unwrap_err().to_string();
    assert_eq!(error, r#"S3 put "log/segment" failed: HTTP 403 SignatureDoesNotMatch"#);

    credentials.replace(&temporary("second-token"));
    let error = s3.list("log").unwrap_err().to_string();
    assert_eq!(error, r#"S3 list "log" failed: HTTP 403 SignatureDoesNotMatch"#);
    let tokens = tokens.lock().unwrap().clone();
    assert_eq!(tokens, [Some("first-token".into()), Some("second-token".into())]);
}
