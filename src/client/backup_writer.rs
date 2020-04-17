use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{format_err, Error};
use chrono::{DateTime, Utc};
use futures::*;
use futures::stream::Stream;
use futures::future::AbortHandle;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

use proxmox::tools::digest_to_hex;

use super::merge_known_chunks::{MergedChunkInfo, MergeKnownChunks};
use crate::backup::*;

use super::{HttpClient, H2Client};

pub struct BackupWriter {
    h2: H2Client,
    abort: AbortHandle,
    verbose: bool,
}

impl Drop for BackupWriter {

    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub struct BackupStats {
    pub size: u64,
    pub csum: [u8; 32],
}

impl BackupWriter {

    fn new(h2: H2Client, abort: AbortHandle, verbose: bool) -> Arc<Self> {
        Arc::new(Self { h2, abort, verbose })
    }

    pub async fn start(
        client: HttpClient,
        datastore: &str,
        backup_type: &str,
        backup_id: &str,
        backup_time: DateTime<Utc>,
        debug: bool,
    ) -> Result<Arc<BackupWriter>, Error> {

        let param = json!({
            "backup-type": backup_type,
            "backup-id": backup_id,
            "backup-time": backup_time.timestamp(),
            "store": datastore,
            "debug": debug
        });

        let req = HttpClient::request_builder(
            client.server(), "GET", "/api2/json/backup", Some(param)).unwrap();

        let (h2, abort) = client.start_h2_connection(req, String::from(PROXMOX_BACKUP_PROTOCOL_ID_V1!())).await?;

        Ok(BackupWriter::new(h2, abort, debug))
    }

    pub async fn get(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.get(path, param).await
    }

    pub async fn put(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.put(path, param).await
    }

    pub async fn post(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.post(path, param).await
    }

    pub async fn upload_post(
        &self,
        path: &str,
        param: Option<Value>,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<Value, Error> {
        self.h2.upload("POST", path, param, content_type, data).await
    }

    pub async fn send_upload_request(
        &self,
        method: &str,
        path: &str,
        param: Option<Value>,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<h2::client::ResponseFuture, Error> {

        let request = H2Client::request_builder("localhost", method, path, param, Some(content_type)).unwrap();
        let response_future = self.h2.send_request(request, Some(bytes::Bytes::from(data.clone()))).await?;
        Ok(response_future)
    }

    pub async fn upload_put(
        &self,
        path: &str,
        param: Option<Value>,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<Value, Error> {
        self.h2.upload("PUT", path, param, content_type, data).await
    }

    pub async fn finish(self: Arc<Self>) -> Result<(), Error> {
        let h2 = self.h2.clone();

        h2.post("finish", None)
            .map_ok(move |_| {
                self.abort.abort();
            })
            .await
    }

    pub fn cancel(&self) {
        self.abort.abort();
    }

    pub async fn upload_blob<R: std::io::Read>(
        &self,
        mut reader: R,
        file_name: &str,
     ) -> Result<BackupStats, Error> {
        let mut raw_data = Vec::new();
        // fixme: avoid loading into memory
        reader.read_to_end(&mut raw_data)?;

        let csum = openssl::sha::sha256(&raw_data);
        let param = json!({"encoded-size": raw_data.len(), "file-name": file_name });
        let size = raw_data.len() as u64;
        let _value = self.h2.upload("POST", "blob", Some(param), "application/octet-stream", raw_data).await?;
        Ok(BackupStats { size, csum })
    }

    pub async fn upload_blob_from_data(
        &self,
        data: Vec<u8>,
        file_name: &str,
        crypt_config: Option<Arc<CryptConfig>>,
        compress: bool,
        sign_only: bool,
     ) -> Result<BackupStats, Error> {

        let blob = if let Some(ref crypt_config) = crypt_config {
            if sign_only {
                DataBlob::create_signed(&data, crypt_config, compress)?
            } else {
                DataBlob::encode(&data, Some(crypt_config), compress)?
            }
        } else {
            DataBlob::encode(&data, None, compress)?
        };

        let raw_data = blob.into_inner();
        let size = raw_data.len() as u64;

        let csum = openssl::sha::sha256(&raw_data);
        let param = json!({"encoded-size": size, "file-name": file_name });
        let _value = self.h2.upload("POST", "blob", Some(param), "application/octet-stream", raw_data).await?;
        Ok(BackupStats { size, csum })
    }

    pub async fn upload_blob_from_file<P: AsRef<std::path::Path>>(
        &self,
        src_path: P,
        file_name: &str,
        crypt_config: Option<Arc<CryptConfig>>,
        compress: bool,
     ) -> Result<BackupStats, Error> {

        let src_path = src_path.as_ref();

        let mut file = tokio::fs::File::open(src_path)
            .await
            .map_err(|err| format_err!("unable to open file {:?} - {}", src_path, err))?;

        let mut contents = Vec::new();

        file.read_to_end(&mut contents)
            .await
            .map_err(|err| format_err!("unable to read file {:?} - {}", src_path, err))?;

        let blob = DataBlob::encode(&contents, crypt_config.as_ref().map(AsRef::as_ref), compress)?;
        let raw_data = blob.into_inner();
        let size = raw_data.len() as u64;
        let csum = openssl::sha::sha256(&raw_data);
        let param = json!({
            "encoded-size": size,
            "file-name": file_name,
        });
        self.h2.upload("POST", "blob", Some(param), "application/octet-stream", raw_data).await?;
        Ok(BackupStats { size, csum })
    }

    pub async fn upload_stream(
        &self,
        archive_name: &str,
        stream: impl Stream<Item = Result<bytes::BytesMut, Error>>,
        prefix: &str,
        fixed_size: Option<u64>,
        crypt_config: Option<Arc<CryptConfig>>,
    ) -> Result<BackupStats, Error> {
        let known_chunks = Arc::new(Mutex::new(HashSet::new()));

        let mut param = json!({ "archive-name": archive_name });
        if let Some(size) = fixed_size {
            param["size"] = size.into();
        }

        let index_path = format!("{}_index", prefix);
        let close_path = format!("{}_close", prefix);

        self.download_chunk_list(&index_path, archive_name, known_chunks.clone()).await?;

        let wid = self.h2.post(&index_path, Some(param)).await?.as_u64().unwrap();

        let (chunk_count, size, duration, speed, csum) =
            Self::upload_chunk_info_stream(
                self.h2.clone(),
                wid,
                stream,
                &prefix,
                known_chunks.clone(),
                crypt_config,
                self.verbose,
            )
            .await?;

        println!("{}: Uploaded {} bytes as {} chunks in {} seconds ({} MB/s).", archive_name, size, chunk_count, duration.as_secs(), speed);
        if chunk_count > 0 {
            println!("{}: Average chunk size was {} bytes.", archive_name, size/chunk_count);
            println!("{}: Time per request: {} microseconds.", archive_name, (duration.as_micros())/(chunk_count as u128));
        }

        let param = json!({
            "wid": wid ,
            "chunk-count": chunk_count,
            "size": size,
            "csum": proxmox::tools::digest_to_hex(&csum),
        });
        let _value = self.h2.post(&close_path, Some(param)).await?;
        Ok(BackupStats {
            size: size as u64,
            csum,
        })
    }

    fn response_queue() -> (
        mpsc::Sender<h2::client::ResponseFuture>,
        oneshot::Receiver<Result<(), Error>>
    ) {
        let (verify_queue_tx, verify_queue_rx) = mpsc::channel(100);
        let (verify_result_tx, verify_result_rx) = oneshot::channel();

        // FIXME: check if this works as expected as replacement for the combinator below?
        // tokio::spawn(async move {
        //     let result: Result<(), Error> = (async move {
        //         while let Some(response) = verify_queue_rx.recv().await {
        //             match H2Client::h2api_response(response.await?).await {
        //                 Ok(result) => println!("RESPONSE: {:?}", result),
        //                 Err(err) => bail!("pipelined request failed: {}", err),
        //             }
        //         }
        //         Ok(())
        //     }).await;
        //     let _ignore_closed_channel = verify_result_tx.send(result);
        // });
        // old code for reference?
        tokio::spawn(
            verify_queue_rx
                .map(Ok::<_, Error>)
                .try_for_each(|response: h2::client::ResponseFuture| {
                    response
                        .map_err(Error::from)
                        .and_then(H2Client::h2api_response)
                        .map_ok(|result| println!("RESPONSE: {:?}", result))
                        .map_err(|err| format_err!("pipelined request failed: {}", err))
                })
                .map(|result| {
                      let _ignore_closed_channel = verify_result_tx.send(result);
                })
        );

        (verify_queue_tx, verify_result_rx)
    }

    fn append_chunk_queue(h2: H2Client, wid: u64, path: String, verbose: bool) -> (
        mpsc::Sender<(MergedChunkInfo, Option<h2::client::ResponseFuture>)>,
        oneshot::Receiver<Result<(), Error>>,
    ) {
        let (verify_queue_tx, verify_queue_rx) = mpsc::channel(64);
        let (verify_result_tx, verify_result_rx) = oneshot::channel();

        let h2_2 = h2.clone();

        // FIXME: async-block-ify this code!
        tokio::spawn(
            verify_queue_rx
                .map(Ok::<_, Error>)
                .and_then(move |(merged_chunk_info, response): (MergedChunkInfo, Option<h2::client::ResponseFuture>)| {
                    match (response, merged_chunk_info) {
                        (Some(response), MergedChunkInfo::Known(list)) => {
                            future::Either::Left(
                                response
                                    .map_err(Error::from)
                                    .and_then(H2Client::h2api_response)
                                    .and_then(move |_result| {
                                        future::ok(MergedChunkInfo::Known(list))
                                    })
                            )
                        }
                        (None, MergedChunkInfo::Known(list)) => {
                            future::Either::Right(future::ok(MergedChunkInfo::Known(list)))
                        }
                        _ => unreachable!(),
                    }
                })
                .merge_known_chunks()
                .and_then(move |merged_chunk_info| {
                    match merged_chunk_info {
                        MergedChunkInfo::Known(chunk_list) => {
                            let mut digest_list = vec![];
                            let mut offset_list = vec![];
                            for (offset, digest) in chunk_list {
                                digest_list.push(digest_to_hex(&digest));
                                offset_list.push(offset);
                            }
                            if verbose { println!("append chunks list len ({})", digest_list.len()); }
                            let param = json!({ "wid": wid, "digest-list": digest_list, "offset-list": offset_list });
                            let request = H2Client::request_builder("localhost", "PUT", &path, None, Some("application/json")).unwrap();
                            let param_data = bytes::Bytes::from(param.to_string().into_bytes());
                            let upload_data = Some(param_data);
                            h2_2.send_request(request, upload_data)
                                .and_then(move |response| {
                                    response
                                        .map_err(Error::from)
                                        .and_then(H2Client::h2api_response)
                                        .map_ok(|_| ())
                                })
                                .map_err(|err| format_err!("pipelined request failed: {}", err))
                        }
                        _ => unreachable!(),
                    }
                })
                .try_for_each(|_| future::ok(()))
                .map(|result| {
                      let _ignore_closed_channel = verify_result_tx.send(result);
                })
        );

        (verify_queue_tx, verify_result_rx)
    }

    pub async fn download_chunk_list(
        &self,
        path: &str,
        archive_name: &str,
        known_chunks: Arc<Mutex<HashSet<[u8;32]>>>,
    ) -> Result<(), Error> {

        let param = json!({ "archive-name": archive_name });
        let request = H2Client::request_builder("localhost", "GET", path, Some(param), None).unwrap();

        let h2request = self.h2.send_request(request, None).await?;
        let resp = h2request.await?;

        let status = resp.status();

        if !status.is_success() {
            H2Client::h2api_response(resp).await?; // raise error
            unreachable!();
        }

        let mut body = resp.into_body();
        let mut flow_control = body.flow_control().clone();

        let mut stream = DigestListDecoder::new(body.map_err(Error::from));

        while let Some(chunk) = stream.try_next().await? {
            let _ = flow_control.release_capacity(chunk.len());
            known_chunks.lock().unwrap().insert(chunk);
        }

        if self.verbose {
            println!("{}: known chunks list length is {}", archive_name, known_chunks.lock().unwrap().len());
        }

        Ok(())
    }

    fn upload_chunk_info_stream(
        h2: H2Client,
        wid: u64,
        stream: impl Stream<Item = Result<bytes::BytesMut, Error>>,
        prefix: &str,
        known_chunks: Arc<Mutex<HashSet<[u8;32]>>>,
        crypt_config: Option<Arc<CryptConfig>>,
        verbose: bool,
    ) -> impl Future<Output = Result<(usize, usize, std::time::Duration, usize, [u8; 32]), Error>> {

        let repeat = Arc::new(AtomicUsize::new(0));
        let repeat2 = repeat.clone();

        let stream_len = Arc::new(AtomicUsize::new(0));
        let stream_len2 = stream_len.clone();

        let append_chunk_path = format!("{}_index", prefix);
        let upload_chunk_path = format!("{}_chunk", prefix);
        let is_fixed_chunk_size = prefix == "fixed";

        let (upload_queue, upload_result) =
            Self::append_chunk_queue(h2.clone(), wid, append_chunk_path.to_owned(), verbose);

        let start_time = std::time::Instant::now();

        let index_csum = Arc::new(Mutex::new(Some(openssl::sha::Sha256::new())));
        let index_csum_2 = index_csum.clone();

        stream
            .and_then(move |data| {

                let chunk_len = data.len();

                repeat.fetch_add(1, Ordering::SeqCst);
                let offset = stream_len.fetch_add(chunk_len, Ordering::SeqCst) as u64;

                let mut chunk_builder = DataChunkBuilder::new(data.as_ref())
                    .compress(true);

                if let Some(ref crypt_config) = crypt_config {
                    chunk_builder = chunk_builder.crypt_config(crypt_config);
                }

                let mut known_chunks = known_chunks.lock().unwrap();
                let digest = chunk_builder.digest();

                let mut guard = index_csum.lock().unwrap();
                let csum = guard.as_mut().unwrap();

                let chunk_end = offset + chunk_len as u64;

                if !is_fixed_chunk_size { csum.update(&chunk_end.to_le_bytes()); }
                csum.update(digest);

                let chunk_is_known = known_chunks.contains(digest);
                if chunk_is_known {
                    future::ok(MergedChunkInfo::Known(vec![(offset, *digest)]))
                } else {
                    known_chunks.insert(*digest);
                    future::ready(chunk_builder
                        .build()
                        .map(move |(chunk, digest)| MergedChunkInfo::New(ChunkInfo {
                            chunk,
                            digest,
                            chunk_len: chunk_len as u64,
                            offset,
                        }))
                    )
                }
            })
            .merge_known_chunks()
            .try_for_each(move |merged_chunk_info| {

                if let MergedChunkInfo::New(chunk_info) = merged_chunk_info {
                    let offset = chunk_info.offset;
                    let digest = chunk_info.digest;
                    let digest_str = digest_to_hex(&digest);

                    if verbose {
                        println!("upload new chunk {} ({} bytes, offset {})", digest_str,
                                 chunk_info.chunk_len, offset);
                    }

                    let chunk_data = chunk_info.chunk.into_inner();
                    let param = json!({
                        "wid": wid,
                        "digest": digest_str,
                        "size": chunk_info.chunk_len,
                        "encoded-size": chunk_data.len(),
                    });

                    let ct = "application/octet-stream";
                    let request = H2Client::request_builder("localhost", "POST", &upload_chunk_path, Some(param), Some(ct)).unwrap();
                    let upload_data = Some(bytes::Bytes::from(chunk_data));

                    let new_info = MergedChunkInfo::Known(vec![(offset, digest)]);

                    let mut upload_queue = upload_queue.clone();
                    future::Either::Left(h2
                        .send_request(request, upload_data)
                        .and_then(move |response| async move {
                            upload_queue
                                .send((new_info, Some(response)))
                                .await
                                .map_err(|err| format_err!("failed to send to upload queue: {}", err))
                        })
                    )
                } else {
                    let mut upload_queue = upload_queue.clone();
                    future::Either::Right(async move {
                        upload_queue
                            .send((merged_chunk_info, None))
                            .await
                            .map_err(|err| format_err!("failed to send to upload queue: {}", err))
                    })
                }
            })
            .then(move |result| async move {
                upload_result.await?.and(result)
            }.boxed())
            .and_then(move |_| {
                let repeat = repeat2.load(Ordering::SeqCst);
                let stream_len = stream_len2.load(Ordering::SeqCst);
                let speed = ((stream_len*1_000_000)/(1024*1024))/(start_time.elapsed().as_micros() as usize);

                let mut guard = index_csum_2.lock().unwrap();
                let csum = guard.take().unwrap().finish();

                futures::future::ok((repeat, stream_len, start_time.elapsed(), speed, csum))
            })
    }

    pub async fn upload_speedtest(&self) -> Result<usize, Error> {

        let mut data = vec![];
        // generate pseudo random byte sequence
        for i in 0..1024*1024 {
            for j in 0..4 {
                let byte = ((i >> (j<<3))&0xff) as u8;
                data.push(byte);
            }
        }

        let item_len = data.len();

        let mut repeat = 0;

        let (upload_queue, upload_result) = Self::response_queue();

        let start_time = std::time::Instant::now();

        loop {
            repeat += 1;
            if start_time.elapsed().as_secs() >= 5 {
                break;
            }

            let mut upload_queue = upload_queue.clone();

            println!("send test data ({} bytes)", data.len());
            let request = H2Client::request_builder("localhost", "POST", "speedtest", None, None).unwrap();
            let request_future = self.h2.send_request(request, Some(bytes::Bytes::from(data.clone()))).await?;

            upload_queue.send(request_future).await?;
        }

        drop(upload_queue); // close queue

        let _ = upload_result.await?;

        println!("Uploaded {} chunks in {} seconds.", repeat, start_time.elapsed().as_secs());
        let speed = ((item_len*1_000_000*(repeat as usize))/(1024*1024))/(start_time.elapsed().as_micros() as usize);
        println!("Time per request: {} microseconds.", (start_time.elapsed().as_micros())/(repeat as u128));

        Ok(speed)
    }
}
