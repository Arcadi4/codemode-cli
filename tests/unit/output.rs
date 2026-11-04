use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::{Value, json};

use crate::output::{self, OutputKind};

fn render(result: Value, kind: OutputKind, json_output: bool) -> String {
    let mut bytes = Vec::new();
    output::render(result, kind, json_output, &mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn assert_artifact(descriptor: &Value, expected: &[u8]) {
    let path = Path::new(descriptor["path"].as_str().unwrap());
    assert!(path.is_absolute());
    assert_eq!(fs::read(path).unwrap(), expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn explicit_operation_preserves_colliding_extension_fields_and_distinct_text() {
    let payload = json!({
        "structuredContent":{"answer":42},
        "messages":[],
        "content":[
            {"type":"text","text":" \t"},
            {"type":"image","mimeType":"image/png","data":BASE64_STANDARD.encode(b"image"),"path":"server-defined"}
        ]
    });
    let result: Value = serde_json::from_str(&render(payload, OutputKind::Tool, false)).unwrap();
    assert_eq!(result["content"][0]["text"], " \t");
    assert_eq!(result["content"][1]["path"], "server-defined");
    assert_eq!(result["messages"], json!([]));
    assert_artifact(&result["content"][1]["data"], b"image");
}

#[test]
fn resource_text_and_binary_are_both_exposed_without_overwriting_metadata() {
    let result: Value = serde_json::from_str(&render(json!({"contents":[{
        "uri":"file://report","text":"caption","blob":BASE64_STANDARD.encode(b"binary"),"path":"server-field"
    }]}), OutputKind::Resources, true)).unwrap();
    assert_eq!(result["contents"][0]["text"], "caption");
    assert_eq!(result["contents"][0]["path"], "server-field");
    assert_artifact(&result["contents"][0]["blob"], b"binary");

    let mut out = Vec::new();
    let error = output::render(
        json!({"content":[{"type":"text","text":123}]}),
        OutputKind::Tool,
        false,
        &mut out,
    )
    .unwrap_err();
    assert!(error.to_string().contains("string text"));
    assert!(
        out.is_empty(),
        "invalid content must not leak partial success output"
    );
}

#[test]
fn closed_pipelines_preserve_broken_pipe_in_plain_and_json_modes() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for json_output in [false, true] {
        let error = output::render(
            json!({"content":[{"type":"text","text":"data"}]}),
            OutputKind::Tool,
            json_output,
            &mut Closed,
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
