use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SplitTask {
    pub task_id: String,
    pub state: u32,
    pub proof_id: String,
    pub base_dir: String,
    pub elf_path: String,
    pub seg_path: String,
    pub public_input_path: String,
    pub private_input_path: String,
    pub output_path: String,
    pub args: String,
    pub block_no: u64,
    pub seg_size: u32,
    pub start_ts: u64,
    pub finish_ts: u64,
    pub node_info: String,
    pub total_steps: u64,
    pub recepit_inputs_path: String,
}

impl Clone for SplitTask {
    fn clone(&self) -> Self {
        SplitTask {
            task_id: self.task_id.clone(),
            state: self.state,
            proof_id: self.proof_id.clone(),
            base_dir: self.base_dir.clone(),
            elf_path: self.elf_path.clone(),
            seg_path: self.seg_path.clone(),
            public_input_path: self.public_input_path.clone(),
            private_input_path: self.private_input_path.clone(),
            output_path: self.output_path.clone(),
            args: self.args.clone(),
            block_no: self.block_no,
            seg_size: self.seg_size,
            start_ts: self.start_ts,
            finish_ts: self.finish_ts,
            node_info: self.node_info.clone(),
            total_steps: self.total_steps,
            recepit_inputs_path: self.recepit_inputs_path.clone(),
        }
    }
}
