#![cfg(feature = "openssl")]
use std::io;

use actix_codec::{AsyncRead, AsyncWrite};
use actix_http_test::TestServer;
use actix_server::ssl::OpensslAcceptor;
use actix_server_config::ServerConfig;
use actix_service::{factory_fn_cfg, pipeline_factory, service_fn2, ServiceFactory};

use bytes::{Bytes, BytesMut};
use futures::future::{err, ok, ready};
use futures::stream::{once, Stream, StreamExt};
use open_ssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

use actix_http::error::{ErrorBadRequest, PayloadError};
use actix_http::http::header::{self, HeaderName, HeaderValue};
use actix_http::http::{Method, StatusCode, Version};
use actix_http::httpmessage::HttpMessage;
use actix_http::{body, Error, HttpService, Request, Response};

async fn load_body<S>(stream: S) -> Result<BytesMut, PayloadError>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    let body = stream
        .map(|res| match res {
            Ok(chunk) => chunk,
            Err(_) => panic!(),
        })
        .fold(BytesMut::new(), move |mut body, chunk| {
            body.extend_from_slice(&chunk);
            ready(body)
        })
        .await;

    Ok(body)
}

fn ssl_acceptor<T: AsyncRead + AsyncWrite>() -> io::Result<OpensslAcceptor<T, ()>> {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("../tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("../tests/cert.pem")
        .unwrap();
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    builder.set_alpn_protos(b"\x02h2")?;
    Ok(OpensslAcceptor::new(builder.build()))
}

#[actix_rt::test]
async fn test_h2() -> io::Result<()> {
    let openssl = ssl_acceptor()?;
    let srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| ok::<_, Error>(Response::Ok().finish()))
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

#[actix_rt::test]
async fn test_h2_1() -> io::Result<()> {
    let openssl = ssl_acceptor()?;
    let srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .finish(|req: Request| {
                    assert!(req.peer_addr().is_some());
                    assert_eq!(req.version(), Version::HTTP_2);
                    ok::<_, Error>(Response::Ok().finish())
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

#[actix_rt::test]
async fn test_h2_body() -> io::Result<()> {
    let data = "HELLOWORLD".to_owned().repeat(64 * 1024);
    let openssl = ssl_acceptor()?;
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|mut req: Request<_>| {
                    async move {
                        let body = load_body(req.take_payload()).await?;
                        Ok::<_, Error>(Response::Ok().body(body))
                    }
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send_body(data.clone()).await.unwrap();
    assert!(response.status().is_success());

    let body = srv.load_body(response).await.unwrap();
    assert_eq!(&body, data.as_bytes());
    Ok(())
}

#[actix_rt::test]
async fn test_h2_content_length() {
    let openssl = ssl_acceptor().unwrap();

    let srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|req: Request| {
                    let indx: usize = req.uri().path()[1..].parse().unwrap();
                    let statuses = [
                        StatusCode::NO_CONTENT,
                        StatusCode::CONTINUE,
                        StatusCode::SWITCHING_PROTOCOLS,
                        StatusCode::PROCESSING,
                        StatusCode::OK,
                        StatusCode::NOT_FOUND,
                    ];
                    ok::<_, ()>(Response::new(statuses[indx]))
                })
                .map_err(|_| ()),
        )
    });

    let header = HeaderName::from_static("content-length");
    let value = HeaderValue::from_static("0");

    {
        for i in 0..4 {
            let req = srv
                .request(Method::GET, srv.surl(&format!("/{}", i)))
                .send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);

            let req = srv
                .request(Method::HEAD, srv.surl(&format!("/{}", i)))
                .send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);
        }

        for i in 4..6 {
            let req = srv
                .request(Method::GET, srv.surl(&format!("/{}", i)))
                .send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), Some(&value));
        }
    }
}

#[actix_rt::test]
async fn test_h2_headers() {
    let data = STR.repeat(10);
    let data2 = data.clone();
    let openssl = ssl_acceptor().unwrap();

    let mut srv = TestServer::start(move || {
        let data = data.clone();
        pipeline_factory(openssl
            .clone()
            .map_err(|e| println!("Openssl error: {}", e)))
            .and_then(
        HttpService::build().h2(move |_| {
            let mut builder = Response::Ok();
            for idx in 0..90 {
                builder.header(
                    format!("X-TEST-{}", idx).as_str(),
                    "TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST ",
                );
            }
            ok::<_, ()>(builder.body(data.clone()))
        }).map_err(|_| ()))
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from(data2));
}

const STR: &str = "Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World";

#[actix_rt::test]
async fn test_h2_body2() {
    let openssl = ssl_acceptor().unwrap();
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| ok::<_, ()>(Response::Ok().body(STR)))
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[actix_rt::test]
async fn test_h2_head_empty() {
    let openssl = ssl_acceptor().unwrap();
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .finish(|_| ok::<_, ()>(Response::Ok().body(STR)))
                .map_err(|_| ()),
        )
    });

    let response = srv.shead("/").send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert!(bytes.is_empty());
}

#[actix_rt::test]
async fn test_h2_head_binary() {
    let openssl = ssl_acceptor().unwrap();
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| {
                    ok::<_, ()>(
                        Response::Ok().content_length(STR.len() as u64).body(STR),
                    )
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.shead("/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert!(bytes.is_empty());
}

#[actix_rt::test]
async fn test_h2_head_binary2() {
    let openssl = ssl_acceptor().unwrap();
    let srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| ok::<_, ()>(Response::Ok().body(STR)))
                .map_err(|_| ()),
        )
    });

    let response = srv.shead("/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }
}

#[actix_rt::test]
async fn test_h2_body_length() {
    let openssl = ssl_acceptor().unwrap();
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| {
                    let body = once(ok(Bytes::from_static(STR.as_ref())));
                    ok::<_, ()>(
                        Response::Ok()
                            .body(body::SizedStream::new(STR.len() as u64, body)),
                    )
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[actix_rt::test]
async fn test_h2_body_chunked_explicit() {
    let openssl = ssl_acceptor().unwrap();
    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| {
                    let body = once(ok::<_, Error>(Bytes::from_static(STR.as_ref())));
                    ok::<_, ()>(
                        Response::Ok()
                            .header(header::TRANSFER_ENCODING, "chunked")
                            .streaming(body),
                    )
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());
    assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));

    // read response
    let bytes = srv.load_body(response).await.unwrap();

    // decode
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[actix_rt::test]
async fn test_h2_response_http_error_handling() {
    let openssl = ssl_acceptor().unwrap();

    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(factory_fn_cfg(|_: &ServerConfig| {
                    ok::<_, ()>(service_fn2(|_| {
                        let broken_header = Bytes::from_static(b"\0\0\0");
                        ok::<_, ()>(
                            Response::Ok()
                                .header(header::CONTENT_TYPE, broken_header)
                                .body(STR),
                        )
                    }))
                }))
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"failed to parse header value"));
}

#[actix_rt::test]
async fn test_h2_service_error() {
    let openssl = ssl_acceptor().unwrap();

    let mut srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .h2(|_| err::<Response, Error>(ErrorBadRequest("error")))
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"error"));
}

#[actix_rt::test]
async fn test_h2_on_connect() {
    let openssl = ssl_acceptor().unwrap();

    let srv = TestServer::start(move || {
        pipeline_factory(
            openssl
                .clone()
                .map_err(|e| println!("Openssl error: {}", e)),
        )
        .and_then(
            HttpService::build()
                .on_connect(|_| 10usize)
                .h2(|req: Request| {
                    assert!(req.extensions().contains::<usize>());
                    ok::<_, ()>(Response::Ok().finish())
                })
                .map_err(|_| ()),
        )
    });

    let response = srv.sget("/").send().await.unwrap();
    assert!(response.status().is_success());
}
