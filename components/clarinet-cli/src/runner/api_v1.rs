use super::vendor::deno_cli::compat;
use super::vendor::deno_cli::create_main_worker;
use super::vendor::deno_cli::ops;
use super::vendor::deno_cli::proc_state::ProcState;
use super::vendor::deno_cli::tools::test::{TestEventSender, TestMode, TestSpecifierOptions};
use super::vendor::deno_runtime::ops::io::Stdio;
use super::vendor::deno_runtime::ops::io::StdioPipe;
use super::vendor::deno_runtime::permissions::Permissions;
use super::DeploymentCache;
use super::SessionArtifacts;
use clarinet_deployments::update_session_with_contracts_executions;
use clarity_repl::clarity::vm::analysis::contract_interface_builder::build_contract_interface;
use clarity_repl::clarity::vm::EvaluationResult;
use clarity_repl::repl::Session;
use clarity_repl::utils;
use deno_core::error::AnyError;
use deno_core::located_script_name;
use deno_core::serde_json::{json, Value};
use deno_core::{op, Extension};
use deno_core::{ModuleSpecifier, OpState};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Sender};

pub enum ClarinetTestEvent {
    SessionTerminated(SessionArtifacts),
}

pub async fn run_bridge(
    program_state: ProcState,
    permissions: Permissions,
    specifier: ModuleSpecifier,
    _mode: TestMode,
    options: TestSpecifierOptions,
    channel: TestEventSender,
    allow_wallets: bool,
    mut cache: Option<DeploymentCache>,
) -> Result<Vec<SessionArtifacts>, AnyError> {
    let mut custom_extensions = vec![ops::testing::init(channel.clone(), options.filter.clone())];

    // Build Clarinet extenstion
    let mut new_session_decl = new_session::decl();
    new_session_decl.name = "api/v1/new_session";
    let mut load_deployment_decl = load_deployment::decl();
    load_deployment_decl.name = "api/v1/load_deployment";
    let mut terminate_session_decl = terminate_session::decl();
    terminate_session_decl.name = "api/v1/terminate_session";
    let mut mine_block_decl = mine_block::decl();
    mine_block_decl.name = "api/v1/mine_block";
    let mut mine_empty_blocks_decl = mine_empty_blocks::decl();
    mine_empty_blocks_decl.name = "api/v1/mine_empty_blocks";
    let mut call_read_only_fn_decl = call_read_only_fn::decl();
    call_read_only_fn_decl.name = "api/v1/call_read_only_fn";
    let mut get_assets_maps_decl = get_assets_maps::decl();
    get_assets_maps_decl.name = "api/v1/get_assets_maps";
    let mut deprecation_notice_decl = deprecation_notice::decl();
    deprecation_notice_decl.name = "api/v1/mine_empty_blocks";

    let clarinet = Extension::builder()
        .ops(vec![
            new_session_decl,
            load_deployment_decl,
            terminate_session_decl,
            mine_block_decl,
            mine_empty_blocks_decl,
            call_read_only_fn_decl,
            get_assets_maps_decl,
        ])
        .build();
    custom_extensions.push(clarinet);

    let mut worker = create_main_worker(
        &program_state,
        specifier.clone(),
        permissions,
        custom_extensions,
        Stdio {
            stdin: StdioPipe::Inherit,
            stdout: StdioPipe::File(channel.stdout()),
            stderr: StdioPipe::File(channel.stderr()),
        },
    );

    worker.js_runtime.execute_script(
        &located_script_name!(),
        r#"Deno[Deno.internal].enableTestAndBench()"#,
    )?;

    // let bootstrap_options = options.bootstrap.clone();
    // let mut worker = Self::from_options(main_module, permissions, options);
    // worker.bootstrap(&bootstrap_options);

    let (event_tx, event_rx) = mpsc::channel();

    let sessions: HashMap<u32, (String, Session)> = HashMap::new();
    let mut deployments: HashMap<Option<String>, DeploymentCache> = HashMap::new();
    if let Some(cache) = cache.take() {
        // Using None as key - it will be used as our default deployment
        deployments.insert(None, cache);
    }

    worker.js_runtime.op_state().borrow_mut().put(allow_wallets);
    worker.js_runtime.op_state().borrow_mut().put(deployments);
    worker.js_runtime.op_state().borrow_mut().put(sessions);
    worker.js_runtime.op_state().borrow_mut().put(0u32);
    worker
        .js_runtime
        .op_state()
        .borrow_mut()
        .put::<Sender<ClarinetTestEvent>>(event_tx.clone());
    worker
        .js_runtime
        .op_state()
        .borrow_mut()
        .put::<TestEventSender>(channel);

    // Enable op call tracing in core to enable better debugging of op sanitizer
    // failures.
    if options.trace_ops {
        worker
            .execute_script(&located_script_name!(), "Deno.core.enableOpCallTracing();")
            .unwrap();
    }
    if options.compat_mode {
        worker.execute_side_module(&compat::GLOBAL_URL).await?;
        worker.execute_side_module(&compat::MODULE_URL).await?;

        let use_esm_loader = compat::check_if_should_use_esm_loader(&specifier)?;

        if use_esm_loader {
            worker.execute_side_module(&specifier).await?;
        } else {
            compat::load_cjs_module(
                &mut worker.js_runtime,
                &specifier.to_file_path().unwrap().display().to_string(),
                false,
            )?;
            worker.run_event_loop(false).await?;
        }
    } else {
        // We execute the module module as a side module so that import.meta.main is not set.
        worker.execute_side_module(&specifier).await?;
    }

    worker.dispatch_load_event(&located_script_name!())?;

    let test_result = worker.js_runtime.execute_script(
        &located_script_name!(),
        &format!(
            r#"Deno[Deno.internal].runTests({})"#,
            json!({ "shuffle": options.shuffle }),
        ),
    )?;

    worker.js_runtime.resolve_value(test_result).await?;

    loop {
        if !worker.dispatch_beforeunload_event(&located_script_name!())? {
            break;
        }
        worker.run_event_loop(false).await?;
    }

    worker.dispatch_unload_event(&located_script_name!())?;

    // let execute_result = worker.execute_module(&main_module).await;
    // if let Err(e) = execute_result {
    //     println!("{}", e);
    //     return Err(e);
    // }

    // let execute_result = worker.execute("window.dispatchEvent(new Event('load'))");
    // if let Err(e) = execute_result {
    //     println!("{}", e);
    //     return Err(e);
    // }

    // let execute_result = worker.execute_module(&test_module).await;
    // if let Err(e) = execute_result {
    //     println!("{}", e);
    //     return Err(e);
    // }

    // let execute_result = worker.execute("window.dispatchEvent(new Event('unload'))");
    // if let Err(e) = execute_result {
    //     println!("{}", e);
    //     return Err(e);
    // }

    let mut artifacts = vec![];
    while let Ok(ClarinetTestEvent::SessionTerminated(artifact)) = event_rx.try_recv() {
        artifacts.push(artifact);
    }
    Ok(artifacts)
}

#[op]
pub fn deprecation_notice(_state: &mut OpState, _args: Value, _: ()) -> Result<(), AnyError> {
    println!("{}: clarinet v{} is incompatible with the version of the library being imported in the test files.", red!("error"), option_env!("CARGO_PKG_VERSION").expect("Unable to detect version"));
    println!("The test files should import the latest version.");
    std::process::exit(1);
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionArgs {
    pub name: String,
    pub load_deployment: bool,
    pub deployment_path: Option<String>,
}

#[op]
fn new_session(state: &mut OpState, args: NewSessionArgs) -> Result<String, AnyError> {
    let session_id = {
        let session_id = match state.try_borrow_mut::<u32>() {
            Some(session_id) => session_id,
            None => panic!(),
        };
        *session_id += 1;
        session_id.clone()
    };

    let cache = {
        let caches = state.borrow::<HashMap<Option<String>, DeploymentCache>>();
        let cache = match args.deployment_path {
            Some(deploynent_path) => {
                let mut entry = caches.get(&Some(deploynent_path.clone()));
                if entry.is_none() {
                    let mut default_entry = caches.get(&None);
                    if let Some(default_entry) = default_entry.take() {
                        if default_entry.deployment_path == Some(deploynent_path.clone()) {
                            entry = Some(default_entry);
                        }
                    }
                    if entry.is_none() {
                        // TODO(lgalabru): Ability to specify a deployment plan in tests
                        // https://github.com/hirosystems/clarinet/issues/357
                        println!("{}: feature identified, but is not supported yet. Please comment in https://github.com/hirosystems/clarinet/issues/357", red!("Error"));
                        std::process::exit(1);
                    }
                }
                entry
            }
            None => {
                let mut default_entry = caches.get(&None);
                if let Some(default_entry) = default_entry.take() {
                    Some(default_entry)
                } else {
                    unreachable!();
                }
            }
        };
        cache.unwrap()
    };

    let allow_wallets = state.borrow::<bool>();
    let accounts = if *allow_wallets {
        cache.deployment.genesis.as_ref().unwrap().wallets.clone()
    } else {
        vec![]
    };

    let mut serialized_contracts = vec![];
    let session = if args.load_deployment {
        for (contract_id, artifacts) in cache.contracts_artifacts.iter() {
            serialized_contracts.push(json!({
                "contract_id": contract_id.to_string(),
                "contract_interface": artifacts.interface,
                "dependencies": artifacts.dependencies,
                "source": artifacts.source,
            }));
        }
        cache.session.clone()
    } else {
        cache.session_accounts_only.clone()
    };

    {
        let sessions = match state.try_borrow_mut::<HashMap<u32, (String, Session)>>() {
            Some(sessions) => sessions,
            None => panic!(),
        };
        let _ = sessions.insert(session_id, (args.name, session));
    }

    Ok(json!({
        "session_id": session_id,
        "accounts": accounts.iter().map(|a| json!({
            "address": a.address.to_string(),
            "balance": u64::try_from(a.balance)
                .expect("u128 unsupported at the moment, please open an issue."),
            "name": a.name.to_string(),
          })).collect::<Vec<_>>(),
        "contracts": serialized_contracts,
    })
    .to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadDeploymentArgs {
    session_id: u32,
    deployment_path: Option<String>,
}

#[op]
fn load_deployment(state: &mut OpState, args: LoadDeploymentArgs) -> Result<String, AnyError> {
    // Retrieve deployment
    let deployment = {
        let caches = state.borrow::<HashMap<Option<String>, DeploymentCache>>();
        let cache = caches
            .get(&args.deployment_path)
            .expect("unable to retrieve deployment");
        cache.deployment.clone()
    };

    // Retrieve session
    let sessions = state
        .try_borrow_mut::<HashMap<u32, (String, Session)>>()
        .expect("unable to retrieve sessions");
    let (label, session) = sessions
        .get_mut(&args.session_id)
        .expect("unable to retrieve session");

    // Execute deployment on session
    let results = update_session_with_contracts_executions(session, &deployment, None, true);
    let mut serialized_contracts = vec![];
    for (contract_id, result) in results.into_iter() {
        match result {
            Ok(execution) => {
                if let EvaluationResult::Contract(contract_result) = execution.result {
                    serialized_contracts.push(json!({
                        "contract_id": contract_id.to_string(),
                        "contract_interface": build_contract_interface(&contract_result.contract.analysis),
                        "source": contract_result.contract.code,
                    }))
                }
            }
            Err(_e) => {
                println!(
                    "{}: unable to load deployment {:?} in test {}",
                    red!("Error"),
                    args.deployment_path,
                    label
                );
                std::process::exit(1);
            }
        }
    }

    let allow_wallets = state.borrow::<bool>();
    let accounts = if *allow_wallets {
        deployment.genesis.as_ref().unwrap().wallets.clone()
    } else {
        vec![]
    };

    Ok(json!({
        "session_id": args.session_id,
        "accounts": accounts.iter().map(|a| json!({
            "address": a.address.to_string(),
            "balance": u64::try_from(a.balance)
                .expect("u128 unsupported at the moment, please open an issue."),
            "name": a.name.to_string(),
            })).collect::<Vec<_>>(),
        "contracts": serialized_contracts,
    })
    .to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminateSessionArgs {
    session_id: u32,
}

#[op]
fn terminate_session(state: &mut OpState, args: TerminateSessionArgs) -> Result<bool, AnyError> {
    // Retrieve session
    let session_artifacts = {
        let sessions = state
            .try_borrow_mut::<HashMap<u32, (String, Session)>>()
            .expect("unable to retrieve sessions");
        let (_, mut session) = sessions
            .remove(&args.session_id)
            .expect("unable to retrieve session");

        let mut coverage_reports = vec![];
        coverage_reports.append(&mut session.coverage_reports);

        let mut costs_reports = vec![];
        costs_reports.append(&mut session.costs_reports);

        SessionArtifacts {
            coverage_reports,
            costs_reports,
        }
    };

    let tx = state.borrow::<Sender<ClarinetTestEvent>>();
    let _ = tx.send(ClarinetTestEvent::SessionTerminated(session_artifacts));

    Ok(true)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MineEmptyBlocksArgs {
    session_id: u32,
    count: u32,
}

#[op]
fn mine_empty_blocks(state: &mut OpState, args: MineEmptyBlocksArgs) -> Result<String, AnyError> {
    let block_height = perform_block(state, args.session_id, |_name, session| {
        let block_height = session.advance_chain_tip(args.count);
        Ok(block_height)
    })?;

    Ok(json!({
      "session_id": args.session_id,
      "block_height": block_height,
    })
    .to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallReadOnlyFnArgs {
    session_id: u32,
    sender: String,
    contract: String,
    method: String,
    args: Vec<String>,
}

#[op]
fn call_read_only_fn(state: &mut OpState, args: CallReadOnlyFnArgs) -> Result<String, AnyError> {
    let (result, events) = perform_block(state, args.session_id, |_name, session| {
        let execution = session
            .invoke_contract_call(
                &args.contract,
                &args.method,
                &args.args,
                &args.sender,
                "readonly-calls".into(),
            )
            .unwrap(); // TODO(lgalabru)
        let result = match execution.result {
            EvaluationResult::Snippet(result) => utils::value_to_string(&result.result),
            _ => unreachable!("Contract result from snippet"),
        };
        Ok((result, execution.events))
    })?;
    Ok(json!({
      "session_id": args.session_id,
      "result": result,
      "events": events,
    })
    .to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAssetsMapsArgs {
    session_id: u32,
}

#[op]
fn get_assets_maps(state: &mut OpState, args: GetAssetsMapsArgs) -> Result<String, AnyError> {
    let assets_maps = perform_block(state, args.session_id, |_name, session| {
        let assets_maps = session.get_assets_maps();
        let mut lev1 = BTreeMap::new();
        for (key1, map1) in assets_maps.into_iter() {
            let mut lev2 = BTreeMap::new();
            for (key2, val2) in map1.into_iter() {
                lev2.insert(
                    key2,
                    u64::try_from(val2)
                        .expect("u128 unsupported at the moment, please open an issue."),
                );
            }
            lev1.insert(key1, lev2);
        }
        Ok(lev1)
    })?;
    Ok(json!({
      "session_id": args.session_id,
      "assets": assets_maps,
    })
    .to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MineBlockArgs {
    session_id: u32,
    transactions: Vec<TransactionArgs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionArgs {
    sender: String,
    contract_call: Option<ContractCallArgs>,
    deploy_contract: Option<DeployContractArgs>,
    transfer_stx: Option<TransferSTXArgs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractCallArgs {
    contract: String,
    method: String,
    args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployContractArgs {
    name: String,
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferSTXArgs {
    amount: u64,
    recipient: String,
}

#[op]
fn mine_block(state: &mut OpState, args: MineBlockArgs) -> Result<String, AnyError> {
    let (block_height, receipts) = perform_block(state, args.session_id, |name, session| {
        let initial_tx_sender = session.get_tx_sender();
        let mut receipts = vec![];
        for tx in args.transactions.iter() {
            if let Some(ref args) = tx.contract_call {
                let execution = match session.invoke_contract_call(
                    &args.contract,
                    &args.method,
                    &args.args,
                    &tx.sender,
                    name.into(),
                ) {
                    Ok(res) => res,
                    Err(diagnostics) => {
                        if diagnostics.len() > 0 {
                            // TODO(lgalabru): if CLARINET_BACKTRACE=1
                            // Retrieve the AST (penultimate entry), and the expression id (last entry)
                            println!(
                                "Runtime error: {}::{}({}) -> {}",
                                args.contract,
                                args.method,
                                args.args.join(", "),
                                diagnostics.last().unwrap().message
                            );
                        }
                        continue;
                    }
                };
                let result = match execution.result {
                    EvaluationResult::Snippet(result) => utils::value_to_string(&result.result),
                    _ => unreachable!("Contract result from snippet"),
                };
                receipts.push((result, execution.events));
            } else {
                session.set_tx_sender(tx.sender.clone());
                if let Some(ref args) = tx.deploy_contract {
                    let execution = session
                        .interpret(
                            args.code.clone(),
                            Some(args.name.clone()),
                            None,
                            false,
                            Some(name.into()),
                            None,
                        )
                        .unwrap(); // TODO(lgalabru)
                    let result = match execution.result {
                        EvaluationResult::Snippet(result) => format!("{}", result.result),
                        _ => unreachable!("Contract result from snippet"),
                    };
                    receipts.push((result, execution.events));
                } else if let Some(ref args) = tx.transfer_stx {
                    let snippet = format!(
                        "(stx-transfer? u{} tx-sender '{})",
                        args.amount, args.recipient
                    );
                    let execution = session
                        .interpret(snippet, None, None, false, Some(name.into()), None)
                        .unwrap(); // TODO(lgalabru)
                    let result = match execution.result {
                        EvaluationResult::Snippet(result) => format!("{}", result.result),
                        _ => unreachable!("Contract result from snippet"),
                    };
                    receipts.push((result, execution.events));
                }
                session.set_tx_sender(initial_tx_sender.clone());
            }
        }
        let block_height = session.advance_chain_tip(1);
        Ok((block_height, receipts))
    })?;

    let payload = json!({
      "session_id": args.session_id,
      "block_height": block_height,
      "receipts":  receipts.iter().map(|r| {
        json!({
          "result": r.0,
          "events": r.1,
        })
      }).collect::<Vec<_>>()
    });

    Ok(payload.to_string())
}

fn perform_block<F, R>(state: &mut OpState, session_id: u32, handler: F) -> Result<R, AnyError>
where
    F: FnOnce(&str, &mut Session) -> Result<R, AnyError>,
{
    let sessions = match state.try_borrow_mut::<HashMap<u32, (String, Session)>>() {
        Some(sessions) => sessions,
        None => panic!(),
    };

    match sessions.get_mut(&session_id) {
        None => {
            println!("Error: unable to retrieve session");
            panic!()
        }
        Some((name, ref mut session)) => handler(name.as_str(), session),
    }
}
