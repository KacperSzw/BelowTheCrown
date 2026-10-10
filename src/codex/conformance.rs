//! Opt-in, local-only payload check against the installed native Codex binary.
use super::*;
use crate::{engine::Engine, paths};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn event(kind: &str, fields: Value) -> String {
    let mut value = fields;
    value["type"] = json!(kind);
    format!("event: {kind}\ndata: {value}\n\n")
}
fn response(sequence: usize, recall: bool, listener: bool) -> String {
    let id = format!("response-{sequence}");
    let mut s = event("response.created", json!({"response":{"id":id}}));
    let item = if recall {
        json!({"type":"function_call","call_id":"own-recall","name":"recall","arguments":"{\"query\":\"daughter\"}"})
    } else {
        let text = if listener {
            json!({"summary":"A test intention.","plan":[]})
        } else {
            json!({"summary":"A test intention.","goals":[],"plan":[{"kind":"guard","seconds":600}],"memories":[],"relationships":[],"acceptPlots":[],"acceptHeardProposals":[]})
        };
        json!({"type":"message","role":"assistant","id":format!("message-{sequence}"),"content":[{"type":"output_text","text":text.to_string()}]})
    };
    s += &event("response.output_item.done", json!({"item":item}));
    s += &event(
        "response.completed",
        json!({"response":{"id":id,"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
    );
    s
}
async fn read_http(socket: &mut tokio::net::TcpStream) -> Result<Value> {
    let mut bytes = vec![];
    let mut buf = [0; 4096];
    loop {
        let read = socket.read(&mut buf).await?;
        ensure!(read > 0, "Mock connection ended before a request");
        bytes.extend_from_slice(&buf[..read]);
        ensure!(bytes.len() < 2_097_152, "Mock request too large");
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..end])?;
            ensure!(
                header.starts_with("POST /responses "),
                "Unexpected mock endpoint"
            );
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>())
                })
                .context("Mock needs Content-Length")??;
            if bytes.len() >= end + 4 + length {
                return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + length])?);
            }
        }
    }
}
fn tool_names(tools: &[Value]) -> Vec<String> {
    let mut names = vec![];
    for tool in tools {
        if let Some(nested) = tool["tools"].as_array() {
            names.extend(tool_names(nested));
        } else {
            names.push(
                tool["name"]
                    .as_str()
                    .unwrap_or_else(|| tool["type"].as_str().unwrap_or("unknown"))
                    .into(),
            );
        }
    }
    names.sort();
    names
}
fn visible_text(item: &Value) -> String {
    item["content"]
        .as_array()
        .map_or_else(String::new, |content| {
            content
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
}
fn check_payload(
    body: &Value,
    lite: bool,
    expected: &ThinkRequest,
    state: &DriverState,
    history: &[ThinkRequest],
) -> Result<()> {
    ensure!(
        body["model"] == MODEL && body["reasoning"]["effort"] == EFFORT,
        "Wrong model or reasoning"
    );
    ensure!(
        body["service_tier"].is_null(),
        "Standard wire tier must be omitted/null"
    );
    let input = body["input"].as_array().context("Missing model input")?;
    let tools = if lite {
        ensure!(
            body["instructions"].is_null() && body["tools"].is_null(),
            "Lite tools/instructions must be input prefixes"
        );
        input
            .iter()
            .find(|i| i["type"] == "additional_tools")
            .context("Missing Lite tool prefix")?["tools"]
            .as_array()
            .context("Missing Lite tools")?
    } else {
        body["tools"].as_array().context("Missing normal tools")?
    };
    let names = tool_names(tools);
    ensure!(
        names
            == if expected.listener {
                Vec::<String>::new()
            } else {
                vec!["recall".into()]
            },
        "Unexpected tool inventory: {names:?}"
    );
    ensure!(
        body["text"]["format"]["strict"] == true,
        "Output schema must be strict"
    );
    ensure!(
        body["text"]["format"]["schema"]
            == if expected.listener {
                interpretation_schema()
            } else {
                decision_schema()
            },
        "Output schema changed"
    );
    let params = state.thread_params(expected)?;
    let base = params["baseInstructions"].as_str().unwrap();
    let developer = params["developerInstructions"].as_str().unwrap();
    if !lite {
        ensure!(
            body["instructions"] == base,
            "Native model instructions were not replaced"
        );
    }
    let messages = input
        .iter()
        .filter(|i| i["type"] == "message")
        .collect::<Vec<_>>();
    let users = messages
        .iter()
        .filter(|i| i["role"] == "user")
        .map(|i| visible_text(i))
        .collect::<Vec<_>>();
    let wire_input = |request: &ThinkRequest| {
        let mut value = request.input.clone();
        if !request.listener
            && let Some(own) = value["self"].as_object_mut()
        {
            own.remove("biography");
            own.remove("personality");
        }
        value.to_string()
    };
    let mut expected_users = history.iter().map(wire_input).collect::<Vec<_>>();
    expected_users.push(wire_input(expected));
    ensure!(
        users == expected_users,
        "Actor context contains missing, foreign or duplicate observations"
    );
    let developers = messages
        .iter()
        .filter(|i| i["role"] == "developer")
        .map(|i| visible_text(i))
        .collect::<Vec<_>>();
    let expected_developers = if lite {
        vec![base.to_owned(), developer.to_owned()]
    } else {
        vec![developer.to_owned()]
    };
    ensure!(
        developers == expected_developers,
        "Unexpected developer context: {:?}",
        developers
            .iter()
            .map(|s| s.chars().take(100).collect::<String>())
            .collect::<Vec<_>>()
    );
    ensure!(
        messages.iter().all(|item| matches!(
            item["role"].as_str(),
            Some("user" | "developer" | "assistant")
        )),
        "Unexpected message role"
    );
    let assistants = messages
        .iter()
        .filter(|item| item["role"] == "assistant")
        .count();
    ensure!(
        if history.is_empty() {
            assistants == 0
        } else {
            assistants > 0
        },
        "Assistant history does not match this actor's thread"
    );
    if expected.actor_id != 102 || expected.listener {
        ensure!(
            !body.to_string().contains("Synthetic private evidence"),
            "Private recall crossed actor or listener contexts"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Requires native Codex 0.160.1 and BTC_CONFORMANCE_CATALOG from private doctor discovery; no paid calls"]
async fn codex_request_conformance() -> Result<()> {
    let catalog = std::fs::read(
        std::env::var_os("BTC_CONFORMANCE_CATALOG")
            .context("Set BTC_CONFORMANCE_CATALOG to a private discovered npc-models.json")?,
    )?;
    for lite in [false, true] {
        let directory = tempfile::tempdir()?;
        let home = paths::private_dir(&directory.path().join("worker/codex"))?;
        // Synthetic credentials are used only with the loopback provider.
        paths::write_private(
            &home.join("auth.json"),
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-loopback-only"}"#,
            false,
        )?;
        let spec = WorkerSpec::prepare(directory.path(), &WorkerOptions::default())?;
        ensure!(
            spec.probe().await? == "codex-cli 0.160.1",
            "Conformance is pinned to Codex 0.160.1"
        );
        let mut descriptor: Value = serde_json::from_slice(&catalog)?;
        descriptor["models"][0]["use_responses_lite"] = json!(lite);
        paths::write_private(
            &home.join("npc-models.json"),
            &serde_json::to_vec(&descriptor)?,
            false,
        )?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let config = format!(
            "model_catalog_json = \"/home/codex/.codex/npc-models.json\"\nmodel_provider = \"loopback\"\n{}\n[model_providers.loopback]\nname = \"loopback\"\nbase_url = \"http://127.0.0.1:{port}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nstream_max_retries = 0\n",
            worker::worker_config()
        );
        paths::write_private(&home.join("config.toml"), config.as_bytes(), false)?;
        let (captured, mut requests) = mpsc::channel(16);
        let server = tokio::spawn(async move {
            // First inference recalls; remaining decisions finish in one response.
            for sequence in 0..5 {
                let (mut socket, _) = listener.accept().await?;
                let body = read_http(&mut socket).await?;
                captured.send(body).await?;
                let sse = response(sequence, sequence == 0, sequence == 2);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    sse.len()
                );
                socket.write_all(header.as_bytes()).await?;
                socket.write_all(sse.as_bytes()).await?;
                socket.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let rpc = Rpc::spawn(spec.command())?;
        rpc.request("initialize",json!({"clientInfo":{"name":"btc_conformance","version":"0.1"},"capabilities":{"experimentalApi":true}})).await?;
        rpc.send(json!({"method":"initialized","params":{}}))
            .await?;
        let (recalls, mut recall_requests) = mpsc::channel::<RecallRequest>(4);
        let driver = CodexDriver::new(
            directory.path().into(),
            "castle",
            "en",
            WorkerOptions::default(),
            recalls,
        )?;
        driver.inner.state.lock().expect("worker state").rpc = Some(rpc.clone());
        let recall = tokio::spawn(async move {
            let request = recall_requests
                .recv()
                .await
                .context("Missing round-trip recall")?;
            ensure!(
                request.actor_id == 102 && request.token.job_id == 1,
                "Recall lost scheduler identity"
            );
            request
                .reply
                .send(Ok(
                    json!([{"id":987,"text":"Synthetic private evidence.","actorId":102}]),
                ))
                .map_err(|_| anyhow::anyhow!("Recall reply abandoned"))?;
            Ok::<_, anyhow::Error>(())
        });
        let result=async {
            let engine=Engine::memory("castle","en",27)?;
            let mut expected=vec![];
            for (index,(actor,listener)) in [(102,false),(101,true),(102,false),(103,false)].into_iter().enumerate() {
                let identity=if listener {None}else{Some(engine.npc_identity(actor)?)};
                let request=ThinkRequest {actor_id:actor,branch:engine.world.branch.clone(),token:DecisionToken{job_id:index as u64+1,context_generation:0},input:json!({"observation":format!("current-{index}"),"self":{"id":actor,"biography":identity.as_ref().map(|i|&i.biography),"personality":identity.as_ref().map(|i|&i.personality)}}),identity,listener};
                let cancellation=CancellationToken::new();
                let mut thinking=driver.think(request.clone(),cancellation.clone());
                let result=tokio::select! { result=&mut thinking=>result, _=tokio::time::sleep(Duration::from_secs(30))=>{cancellation.cancel();let _=thinking.await;bail!("Mock decision timed out")} }?;
                if index==0 {ensure!(result.recalled==vec![987],"Recall evidence missing");} else {ensure!(result.recalled.is_empty(),"Another decision inherited recalled evidence");}
                expected.push(request);
            }
            for (sequence,index) in [0,0,1,2,3].into_iter().enumerate() {
                let body=requests.recv().await.context("Missing captured request")?;
                if sequence==1 {
                    let text=body.to_string(); ensure!(text.contains("own-recall") && text.contains("Synthetic private evidence"),"Recall output missing from round trip");
                } else {
                    let history=if index==2 {&expected[..1]}else{&expected[..0]};
                    check_payload(&body,lite,&expected[index],&driver.inner,history)?;
                }
            }
            recall.await??; server.await??;
            let loaded=rpc.request("thread/loaded/list",json!({})).await?;
            ensure!(loaded["data"].as_array().is_some_and(|data| data.len()==2),"Only the two NPC contexts should remain loaded; listener must retire");
            ensure!(driver.inner.state.lock().expect("worker state").threads.len()==2,"NPC contexts were not reused independently");
            Ok::<_,anyhow::Error>(())
        }.await;
        driver.reset().await;
        result?;
    }
    Ok(())
}
