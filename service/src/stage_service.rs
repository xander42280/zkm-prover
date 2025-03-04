use anyhow::Error;
use common::tls::Config as TlsConfig;
use stage_service::stage_service_server::StageService;
use stage_service::{GenerateProofRequest, GenerateProofResponse};
use stage_service::{GetStatusRequest, GetStatusResponse};
use std::sync::Mutex;

use tonic::{Request, Response, Status};

use crate::config;
use common::file;
use prover::provers;
use std::io::Write;

use ethers::types::Signature;
use std::str::FromStr;

use crate::database;
use crate::metrics;
use crate::stage_worker;

#[allow(clippy::module_inception)]
pub mod stage_service {
    tonic::include_proto!("stage.v1");
}

use lazy_static::lazy_static;
use std::collections::HashMap;

lazy_static! {
    static ref GLOBAL_TASKMAP: Mutex<HashMap<String, i32>> = Mutex::new(HashMap::new());
}

pub struct StageServiceSVC {
    db: database::Database,
    fileserver_url: Option<String>,
    verifier_url: Option<String>,
}

impl StageServiceSVC {
    pub async fn new(config: config::RuntimeConfig) -> anyhow::Result<Self> {
        let tls_config = if config.ca_cert_path.is_some() {
            Some(
                TlsConfig::new(
                    config.ca_cert_path.unwrap(),
                    config.cert_path.unwrap(),
                    config.key_path.unwrap(),
                )
                .await?,
            )
        } else {
            None
        };
        let database_url = config.database_url.as_str();
        let db = database::Database::new(database_url);
        sqlx::migrate!("./migrations").run(&db.db_pool).await?;
        let _ = stage_worker::start(tls_config.clone(), db.clone()).await;
        Ok(StageServiceSVC {
            db,
            fileserver_url: config.fileserver_url.clone(),
            verifier_url: config.verifier_url.clone(),
        })
    }

    pub fn valid_signature(&self, request: &GenerateProofRequest) -> Result<String, Error> {
        let sign_data = match request.block_no {
            Some(block_no) => {
                format!("{}&{}&{}", request.proof_id, block_no, request.seg_size)
            }
            None => {
                format!("{}&{}", request.proof_id, request.seg_size)
            }
        };
        let signature = Signature::from_str(&request.signature)?;
        let recovered = signature.recover(sign_data)?;
        Ok(hex::encode(recovered))
    }
}

#[tonic::async_trait]
impl StageService for StageServiceSVC {
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> tonic::Result<Response<GetStatusResponse>, Status> {
        metrics::record_metrics("stage::get_status", || async {
            let task = self.db.get_stage_task(&request.get_ref().proof_id).await;
            let mut response = stage_service::GetStatusResponse {
                proof_id: request.get_ref().proof_id.clone(),
                ..Default::default()
            };
            if let Ok(task) = task {
                response.status = task.status as u32;
                response.step = task.step;
                let execute_info: Vec<stage::tasks::SplitTask> = self
                    .db
                    .get_prove_task_infos(
                        &request.get_ref().proof_id,
                        stage::tasks::TASK_ITYPE_SPLIT,
                    )
                    .await
                    .unwrap_or_default();
                if !execute_info.is_empty() {
                    response.total_steps = execute_info[0].total_steps;
                }

                let (execute_only, precompile) = if let Some(context) = task.context {
                    match serde_json::from_str::<stage::contexts::GenerateContext>(&context) {
                        Ok(context) => {
                            if task.status == stage_service::Status::Success as i32
                                && !context.output_stream_path.is_empty()
                            {
                                let output_data =
                                    file::new(&context.output_stream_path).read().unwrap();
                                response.output_stream.clone_from(&output_data);
                                if context.precompile {
                                    let receipts_path = format!("{}/receipt/0", context.prove_path);
                                    let receipts_data = file::new(&receipts_path).read().unwrap();
                                    response.receipt = receipts_data;
                                }
                            }
                            (context.execute_only, context.precompile)
                        }
                        Err(_) => (false, false),
                    }
                } else {
                    (false, false)
                };
                if !execute_only && !precompile {
                    if let Some(result) = task.result {
                        response.proof_with_public_inputs = result.into_bytes();
                    }
                    if let Some(fileserver_url) = &self.fileserver_url {
                        response.proof_url = format!(
                            "{}/{}/final/proof_with_public_inputs.json",
                            fileserver_url,
                            request.get_ref().proof_id
                        );
                        response.stark_proof_url = format!(
                            "{}/{}/aggregate/proof_with_public_inputs.json",
                            fileserver_url,
                            request.get_ref().proof_id
                        );
                        response.public_values_url = format!(
                            "{}/{}/aggregate/public_values.json",
                            fileserver_url,
                            request.get_ref().proof_id
                        );
                    }
                    if let Some(verifier_url) = &self.verifier_url {
                        response.solidity_verifier_url.clone_from(verifier_url);
                    }
                }
            }
            Ok(Response::new(response))
        })
        .await
    }

    async fn generate_proof(
        &self,
        request: Request<GenerateProofRequest>,
    ) -> tonic::Result<Response<GenerateProofResponse>, Status> {
        metrics::record_metrics("stage::generate_proof", || async {
            log::info!("[generate_proof] {} start", request.get_ref().proof_id);

            // check seg_size
            if !request.get_ref().precompile
                && !provers::valid_seg_size(request.get_ref().seg_size as usize)
            {
                let response = stage_service::GenerateProofResponse {
                    proof_id: request.get_ref().proof_id.clone(),
                    status: stage_service::Status::InvalidParameter as u32,
                    error_message: format!(
                        "invalid seg_size support [{}-{}]",
                        provers::MIN_SEG_SIZE,
                        provers::MAX_SEG_SIZE
                    ),
                    ..Default::default()
                };
                log::warn!(
                    "[generate_proof] {} invalid seg_size support [{}-{}] {}",
                    request.get_ref().proof_id,
                    request.get_ref().seg_size,
                    provers::MIN_SEG_SIZE,
                    provers::MAX_SEG_SIZE
                );
                return Ok(Response::new(response));
            }
            // check signature
            let user_address: String;
            match self.valid_signature(request.get_ref()) {
                Ok(address) => {
                    // check white list
                    let users = self.db.get_user(&address).await.unwrap();
                    log::info!(
                        "[generate_proof] proof_id:{} address:{:?} exists:{:?}",
                        request.get_ref().proof_id,
                        address,
                        !users.is_empty(),
                    );
                    if users.is_empty() {
                        let response = stage_service::GenerateProofResponse {
                            proof_id: request.get_ref().proof_id.clone(),
                            status: stage_service::Status::InvalidParameter as u32,
                            error_message: "permission denied".to_string(),
                            ..Default::default()
                        };
                        log::warn!(
                            "[generate_proof] {} permission denied",
                            request.get_ref().proof_id,
                        );
                        return Ok(Response::new(response));
                    }
                    user_address = users[0].address.clone();
                }
                Err(e) => {
                    let response = stage_service::GenerateProofResponse {
                        proof_id: request.get_ref().proof_id.clone(),
                        status: stage_service::Status::InvalidParameter as u32,
                        error_message: "invalid signature".to_string(),
                        ..Default::default()
                    };
                    log::warn!(
                        "[generate_proof] {} invalid signature {:?}",
                        request.get_ref().proof_id,
                        e,
                    );
                    return Ok(Response::new(response));
                }
            }

            let base_dir = config::instance().lock().unwrap().base_dir.clone();
            let dir_path = format!("{}/proof/{}", base_dir, request.get_ref().proof_id);
            file::new(&dir_path)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let elf_path = format!("{}/elf", dir_path);
            file::new(&elf_path)
                .write(&request.get_ref().elf_data)
                .map_err(|e| Status::internal(e.to_string()))?;

            let block_no = request.get_ref().block_no.unwrap_or(0u64);
            let block_dir = format!("{}/0_{}", dir_path, block_no);
            file::new(&block_dir)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            for file_block_item in &request.get_ref().block_data {
                let block_path = format!("{}/{}", block_dir, file_block_item.file_name);
                file::new(&block_path)
                    .write(&file_block_item.file_content)
                    .map_err(|e| Status::internal(e.to_string()))?;
            }

            let input_stream_dir = format!("{}/input_stream", dir_path);
            file::new(&input_stream_dir)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;
            let public_input_stream_path = if request.get_ref().public_input_stream.is_empty() {
                "".to_string()
            } else {
                let public_input_stream_path = format!("{}/{}", input_stream_dir, "public_input");
                file::new(&public_input_stream_path)
                    .write(&request.get_ref().public_input_stream)
                    .map_err(|e| Status::internal(e.to_string()))?;
                public_input_stream_path
            };

            let private_input_stream_path = if request.get_ref().private_input_stream.is_empty() {
                "".to_string()
            } else {
                let private_input_stream_path = format!("{}/{}", input_stream_dir, "private_input");
                file::new(&private_input_stream_path)
                    .write(&request.get_ref().private_input_stream)
                    .map_err(|e| Status::internal(e.to_string()))?;
                private_input_stream_path
            };

            let receipt_inputs_path = if request.get_ref().receipt_input.is_empty() {
                "".to_string()
            } else {
                let receipt_inputs_path = format!("{}/{}", input_stream_dir, "receipt_inputs");
                let mut buf = Vec::new();
                bincode::serialize_into(&mut buf, &request.get_ref().receipt_input)
                    .expect("serialization failed");
                file::new(&receipt_inputs_path)
                    .write(&buf)
                    .map_err(|e| Status::internal(e.to_string()))?;
                receipt_inputs_path
            };

            let receipts_path = if request.get_ref().receipt.is_empty() {
                "".to_string()
            } else {
                let receipts_path = format!("{}/{}", input_stream_dir, "receipts");
                let mut buf = Vec::new();
                bincode::serialize_into(&mut buf, &request.get_ref().receipt)
                    .expect("serialization failed");
                file::new(&receipts_path)
                    .write(&buf)
                    .map_err(|e| Status::internal(e.to_string()))?;
                receipts_path
            };

            let output_stream_dir = format!("{}/output_stream", dir_path);
            file::new(&output_stream_dir)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let output_stream_path = format!("{}/{}", output_stream_dir, "output_stream");

            let seg_path = format!("{}/segment", dir_path);
            file::new(&seg_path)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let prove_path = format!("{}/prove", dir_path);
            file::new(&prove_path)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let prove_receipt_path = format!("{}/receipt", prove_path);
            file::new(&prove_receipt_path)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let agg_path = format!("{}/aggregate", dir_path);
            file::new(&agg_path)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;

            let final_dir = format!("{}/final", dir_path);
            file::new(&final_dir)
                .create_dir_all()
                .map_err(|e| Status::internal(e.to_string()))?;
            let final_path = format!("{}/proof_with_public_inputs.json", final_dir);

            let generate_context = stage::contexts::GenerateContext::new(
                &request.get_ref().proof_id,
                &dir_path,
                &elf_path,
                &seg_path,
                &prove_path,
                &agg_path,
                &final_path,
                &public_input_stream_path,
                &private_input_stream_path,
                &output_stream_path,
                block_no,
                request.get_ref().seg_size,
                request.get_ref().execute_only,
                request.get_ref().precompile,
                &receipt_inputs_path,
                &receipts_path,
            );

            let _ = self
                .db
                .insert_stage_task(
                    &request.get_ref().proof_id,
                    &user_address,
                    stage_service::Status::Computing as i32,
                    &serde_json::to_string(&generate_context).unwrap(),
                )
                .await;
            let mut proof_url = match &self.fileserver_url {
                Some(fileserver_url) => format!(
                    "{}/{}/final/proof_with_public_inputs.json",
                    fileserver_url,
                    request.get_ref().proof_id
                ),
                None => "".to_string(),
            };
            let mut stark_proof_url = match &self.fileserver_url {
                Some(fileserver_url) => format!(
                    "{}/{}/aggregate/proof_with_public_inputs.json",
                    fileserver_url,
                    request.get_ref().proof_id
                ),
                None => "".to_string(),
            };
            let mut public_values_url = match &self.fileserver_url {
                Some(fileserver_url) => format!(
                    "{}/{}/aggregate/public_values.json",
                    fileserver_url,
                    request.get_ref().proof_id
                ),
                None => "".to_string(),
            };
            let mut solidity_verifier_url = match &self.verifier_url {
                Some(verifier_url) => verifier_url.clone(),
                None => "".to_string(),
            };
            if request.get_ref().execute_only {
                proof_url = "".to_string();
                stark_proof_url = "".to_string();
                solidity_verifier_url = "".to_string();
                public_values_url = "".to_string();
            }
            let response = stage_service::GenerateProofResponse {
                proof_id: request.get_ref().proof_id.clone(),
                status: stage_service::Status::Computing as u32,
                proof_url,
                stark_proof_url,
                solidity_verifier_url,
                public_values_url,
                ..Default::default()
            };
            log::info!("[generate_proof] {} end", request.get_ref().proof_id);
            Ok(Response::new(response))
        })
        .await
    }
}
