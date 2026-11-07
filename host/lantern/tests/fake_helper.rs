use lantern::daemon::session::handshake;
use lantern_proto::{read_frame_async, write_frame_async, Control, Frame, Kind};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

async fn fake_helper(version: u32) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let hello = read_frame_async(&mut socket).await.unwrap().unwrap();
        assert_eq!(hello.kind, Kind::Control);
        assert_eq!(hello.stream, 0);
        match Control::decode(&hello.payload).unwrap() {
            Control::Hello {
                version: got,
                token,
            } => {
                assert_eq!(got, version);
                assert_eq!(token, TOKEN);
            }
            other => panic!("expected Hello, got {other:?}"),
        }
        let response = if version == 1 {
            Control::Hello {
                version: 1,
                token: String::new(),
            }
        } else {
            Control::Error {
                msg: format!("protocol version mismatch: {version}"),
            }
        };
        write_frame_async(
            &mut socket,
            &Frame {
                kind: Kind::Control,
                stream: 0,
                payload: response.encode(),
            },
        )
        .await
        .unwrap();
    });
    (port, task)
}

#[tokio::test]
async fn session_handshake_against_fake_helper() {
    let (port, helper) = fake_helper(1).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(stream, TOKEN, 1).await.unwrap();
    helper.await.unwrap();
}

#[tokio::test]
async fn session_handshake_rejects_wrong_version() {
    let (port, helper) = fake_helper(2).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let error = handshake(stream, TOKEN, 2).await.unwrap_err().to_string();
    assert!(error.contains("version"), "unexpected error: {error}");
    helper.await.unwrap();
}
