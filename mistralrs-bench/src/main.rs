use candle_core::Device;
use clap::Parser;
use cli_table::{format::Justify, print_stdout, Cell, CellStruct, Style, Table};
use either::Either;
use mistralrs_core::{
    initialize_logging, paged_attn_supported, Constraint, DefaultSchedulerMethod,
    DeviceLayerMapMetadata, DeviceMapMetadata, Loader, LoaderBuilder, MistralRs, MistralRsBuilder,
    ModelDType, ModelSelected, NormalRequest, PagedAttentionConfig, Request, RequestMessage,
    Response, SamplingParams, SchedulerConfig, TokenSource, Usage,
};
use std::fmt::Display;
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tracing::{info, warn};

enum TestName {
    Prompt(usize),
    Gen(usize),
}

impl Display for TestName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            TestName::Prompt(n) => format!("pp {}", n),
            TestName::Gen(n) => format!("tg {}", n),
        };
        write!(f, "{}", name)
    }
}

struct BenchResult {
    usages: Vec<Usage>,
    concurrency: usize,
    test_name: TestName,
}

struct UncertainTokSec {
    mean: f32,
    std_dev: f32,
}

impl Display for UncertainTokSec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3}±{:.3}", self.mean, self.std_dev)
    }
}

fn run_bench(
    mistralrs: Arc<MistralRs>,
    prompt: RequestMessage,
    n_gen: usize,
    concurrency: usize,
    repetitions: usize,
    test_name: TestName,
) -> anyhow::Result<BenchResult> {
    let sampling_params = SamplingParams {
        temperature: Some(0.1),
        top_k: Some(32),
        top_p: Some(0.1),
        top_n_logprobs: 0,
        frequency_penalty: Some(0.1),
        presence_penalty: Some(0.1),
        max_len: Some(n_gen),
        stop_toks: None,
        logits_bias: None,
        n_choices: 1,
    };
    let sender = mistralrs.get_sender().unwrap();
    let (tx, mut rx) = channel(10_000);

    let req = Request::Normal(NormalRequest {
        id: mistralrs.next_request_id(),
        messages: prompt,
        sampling_params: sampling_params.clone(),
        response: tx,
        return_logprobs: false,
        is_streaming: false,
        constraint: Constraint::None,
        suffix: None,
        adapters: None,
    });

    let mut usages = Vec::new();

    for _ in 0..repetitions {
        for _ in 0..concurrency {
            sender
                .blocking_send(req.clone())
                .expect("Expected receiver.");
        }
        for _ in 0..concurrency {
            match rx.blocking_recv() {
                Some(r) => match r {
                    Response::InternalError(e) => {
                        unreachable!("Got an internal error: {e:?}");
                    }
                    Response::ModelError(e, resp) => {
                        unreachable!("Got a model error: {e:?}, response: {resp:?}");
                    }
                    Response::ValidationError(e) => {
                        unreachable!("Got a validation error: {e:?}");
                    }
                    Response::Done(res) => {
                        usages.push(res.usage);
                    }
                    Response::Chunk(_) => unreachable!(),
                    Response::CompletionModelError(_, _) => unreachable!(),
                    Response::CompletionDone(res) => {
                        usages.push(res.usage);
                    }
                    Response::CompletionChunk(_) => unreachable!(),
                },
                None => unreachable!("Expected a Done response, got None",),
            }
        }
    }

    Ok(BenchResult {
        usages,
        concurrency,
        test_name,
    })
}

fn get_tok_s(result: &BenchResult) -> UncertainTokSec {
    let ts_measurements = match result.test_name {
        TestName::Prompt(_) => result
            .usages
            .iter()
            .map(|u| u.avg_prompt_tok_per_sec)
            .collect::<Vec<_>>(),
        TestName::Gen(_) => result
            .usages
            .iter()
            .map(|u| u.avg_compl_tok_per_sec)
            .collect::<Vec<_>>(),
    };
    // Calculate uncertainty
    let mean = ts_measurements.iter().sum::<f32>() / ts_measurements.len() as f32;
    let variance = ts_measurements
        .iter()
        .map(|e| (mean - e).powf(2.))
        .sum::<f32>()
        / ts_measurements.len() as f32;
    let std_dev = variance.sqrt();
    UncertainTokSec { mean, std_dev }
}

fn get_ms_tok(result: &BenchResult) -> UncertainTokSec {
    let ms_tok_measurements = match result.test_name {
        TestName::Prompt(_) => result
            .usages
            .iter()
            .map(|u| 1000. / u.avg_prompt_tok_per_sec)
            .collect::<Vec<_>>(),
        TestName::Gen(_) => result
            .usages
            .iter()
            .map(|u| 1000. / u.avg_compl_tok_per_sec)
            .collect::<Vec<_>>(),
    };
    // Calculate uncertainty
    let mean = ms_tok_measurements.iter().sum::<f32>() / ms_tok_measurements.len() as f32;
    let variance = ms_tok_measurements
        .iter()
        .map(|e| (mean - e).powf(2.))
        .sum::<f32>()
        / ms_tok_measurements.len() as f32;
    let std_dev = variance.sqrt();
    UncertainTokSec { mean, std_dev }
}

fn print_usage(model: &str, device: &Device, results: Vec<BenchResult>) {
    let backend = match device {
        Device::Cpu => "CPU",
        Device::Cuda(_) => "CUDA",
        Device::Metal(_) => "Metal",
    };
    let results: Vec<Vec<CellStruct>> = results
        .into_iter()
        .map(|r| {
            vec![
                model.cell(),
                backend.cell(),
                r.test_name.to_string().cell(),
                get_tok_s(&r).cell().justify(Justify::Right),
                get_ms_tok(&r).cell().justify(Justify::Right),
                r.concurrency.cell().justify(Justify::Right),
                (get_tok_s(&r).mean * r.concurrency as f32)
                    .cell()
                    .justify(Justify::Right),
            ]
        })
        .collect();

    let table = results
        .table()
        .title(vec![
            "model".cell().bold(true),
            // "size".cell().bold(true),
            // "params".cell().bold(true),
            "backend".cell().bold(true),
            // "ngl".cell().bold(true),
            "test".cell().bold(true),
            "t/s".cell().bold(true),
            "ms/t".cell().bold(true),
            "concurrency".cell().bold(true),
            "throughput/s".cell().bold(true),
        ])
        .bold(true);
    print_stdout(table).expect("print table");
}

fn warmup_run(mistralrs: Arc<MistralRs>) {
    let sampling_params = SamplingParams {
        temperature: Some(0.1),
        top_k: Some(32),
        top_p: Some(0.1),
        top_n_logprobs: 0,
        frequency_penalty: Some(0.1),
        presence_penalty: Some(0.1),
        max_len: Some(5),
        stop_toks: None,
        logits_bias: None,
        n_choices: 1,
    };
    let sender = mistralrs.get_sender().unwrap();
    let (tx, mut rx) = channel(10_000);

    let req = Request::Normal(NormalRequest {
        id: mistralrs.next_request_id(),
        messages: RequestMessage::Completion {
            text: "Hello!".to_string(),
            echo_prompt: false,
            best_of: 1,
        },
        sampling_params: sampling_params.clone(),
        response: tx,
        return_logprobs: false,
        is_streaming: false,
        constraint: Constraint::None,
        suffix: None,
        adapters: None,
    });

    sender
        .blocking_send(req.clone())
        .expect("Expected receiver.");

    let _ = rx.blocking_recv();
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Model
    #[clap(subcommand)]
    model: ModelSelected,

    /// Number of prompt tokens to run.
    #[arg(long, short = 'p', default_value_t = 512)]
    n_prompt: usize,

    /// Number of generations tokens to run.
    #[arg(long, short = 'g', default_value_t = 128)]
    n_gen: usize,

    /// Number of concurrent requests to run. Default is 1
    #[clap(short, long, value_parser, value_delimiter = ',')]
    concurrency: Option<Vec<usize>>,

    /// Number of times to repeat each test.
    #[arg(long, short, default_value_t = 5)]
    repetitions: usize,

    /// Number of device layers to load and run on GPU(s). All others will be on the CPU.
    /// If one GPU is used, then this value should be an integer. Otherwise, it follows the following pattern:
    /// ORD:NUM;... Where ORD is a unique device ordinal and NUM is the number of layers for that device.
    #[arg(short, long, value_parser, value_delimiter = ';')]
    num_device_layers: Option<Vec<String>>,

    /// GPU memory to allocate for KV cache with PagedAttention in MBs. If this is not set and the device is CUDA, it will default to
    /// using `pa-gpu-mem-usage` set to `0.9`. PagedAttention is only supported on CUDA and is always automatically activated.
    #[arg(long = "pa-gpu-mem")]
    paged_attn_gpu_mem: Option<usize>,

    /// Percentage of GPU memory to utilize after allocation of KV cache with PagedAttention, from 0 to 1.
    /// If this is not set and the device is CUDA, it will default to `0.9`. PagedAttention is only supported on CUDA and is always automatically activated.
    /// This is always used over `pa-gpu-mem` if both are specified.
    #[arg(long = "pa-gpu-mem-usage")]
    paged_attn_gpu_mem_usage: Option<f32>,

    /// Block size (number of tokens per block) for PagedAttention. If this is not set and the device is CUDA, it will default to 32.
    /// PagedAttention is only supported on CUDA and is always automatically activated.
    #[arg(long = "pa-blk-size")]
    paged_attn_block_size: Option<usize>,

    /// Disable PagedAttention on CUDA.
    #[arg(long = "no_paged_attn", default_value_t = false)]
    no_paged_attn: bool,
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    initialize_logging();

    args.concurrency = Some(args.concurrency.unwrap_or(vec![1]));

    #[cfg(not(feature = "flash-attn"))]
    let use_flash_attn = false;
    #[cfg(feature = "flash-attn")]
    let use_flash_attn = true;

    let loader: Box<dyn Loader> = LoaderBuilder::new(args.model)
        .with_use_flash_attn(use_flash_attn)
        .build()?;
    let model_name = loader.get_id();

    #[cfg(feature = "metal")]
    let device = Device::new_metal(0)?;
    #[cfg(not(feature = "metal"))]
    let device = Device::cuda_if_available(0)?;

    let token_source = TokenSource::CacheToken;
    info!(
        "avx: {}, neon: {}, simd128: {}, f16c: {}",
        candle_core::utils::with_avx(),
        candle_core::utils::with_neon(),
        candle_core::utils::with_simd128(),
        candle_core::utils::with_f16c()
    );
    info!("Sampling method: penalties -> temperature -> topk -> topp -> multinomial");
    if use_flash_attn {
        info!("Using flash attention.");
    }
    if use_flash_attn && loader.get_kind().is_quantized() {
        warn!("Using flash attention with a quantized model has no effect!")
    }
    info!("Model kind is: {}", loader.get_kind().to_string());

    // Parse device mapper
    let mapper = if let Some(device_layers) = args.num_device_layers {
        if device_layers.len() == 1 && device_layers[0].parse::<usize>().is_ok() {
            let layers = device_layers[0].parse::<usize>().unwrap();
            DeviceMapMetadata::from_num_device_layers(vec![DeviceLayerMapMetadata {
                ordinal: 0,
                layers,
            }])
        } else {
            let mut mapping = Vec::new();
            for layer in device_layers {
                let split = layer.splitn(2, ':').collect::<Vec<_>>();
                if split.len() < 2 {
                    panic!("Expected layer to be of format ORD:NUM, got {layer}");
                }
                let ord = split[0]
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("Failed to parse {} as integer.", split[0]));
                let num = split[1]
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("Failed to parse {} as integer.", split[1]));
                for DeviceLayerMapMetadata { ordinal, layers: _ } in &mapping {
                    if *ordinal == ord {
                        panic!("Duplicate ordinal {ord}");
                    }
                }
                mapping.push(DeviceLayerMapMetadata {
                    ordinal: ord,
                    layers: num,
                });
            }
            DeviceMapMetadata::from_num_device_layers(mapping)
        }
    } else {
        DeviceMapMetadata::dummy()
    };

    // Allocate 0.5 GB of CPU memory just as a placeholder.
    // Nothing happens here as we have no `swap_out`, see `_preempt_by_swap`.
    let cache_config = match (
        args.paged_attn_block_size,
        args.paged_attn_gpu_mem,
        args.paged_attn_gpu_mem_usage,
        paged_attn_supported(),
        args.no_paged_attn,
    ) {
        (block_size, None, None, true, false) => Some(PagedAttentionConfig::new(
            block_size,
            512,
            Either::Right(0.9), // NOTE(EricLBuehler): default is to use 90% of memory
        )?),
        (block_size, Some(m), None, true, false) => {
            Some(PagedAttentionConfig::new(block_size, 512, Either::Left(m))?)
        }
        (block_size, None, Some(f), true, false) => Some(PagedAttentionConfig::new(
            block_size,
            512,
            Either::Right(f),
        )?),
        (block_size, Some(_m), Some(f), true, false) => {
            info!("Both memory size and usage were specified, defaulting to the usage value.");
            Some(PagedAttentionConfig::new(
                block_size,
                512,
                Either::Right(f),
            )?)
        }
        (_, _, _, _, _) => None,
    };

    let pipeline = loader.load_model_from_hf(
        None,
        token_source,
        &ModelDType::Auto,
        &device,
        false,
        mapper,
        None,
        cache_config,
    )?;
    info!("Model loaded.");

    let scheduler_config = if cache_config.is_some() {
        SchedulerConfig::PagedAttentionMeta {
            max_num_seqs: *args.concurrency.as_ref().unwrap().iter().max().unwrap(),
            config: pipeline
                .blocking_lock()
                .get_metadata()
                .cache_config
                .as_ref()
                .unwrap()
                .clone(),
        }
    } else {
        SchedulerConfig::DefaultScheduler {
            method: DefaultSchedulerMethod::Fixed(
                (*args.concurrency.as_ref().unwrap().iter().max().unwrap())
                    .try_into()
                    .unwrap(),
            ),
        }
    };
    let mistralrs = MistralRsBuilder::new(pipeline, scheduler_config)
        .with_no_prefix_cache(true)
        .with_disable_eos_stop(true)
        .build();

    info!("Starting warmup run.");
    warmup_run(mistralrs.clone());
    info!("Finished warmup run.");
    info!("Starting benchmarks.");

    for concurrency in args.concurrency.as_ref().unwrap() {
        let mut results = vec![];
        if args.n_gen > 0 {
            let r = run_bench(
                mistralrs.clone(),
                RequestMessage::Completion {
                    text: "Rust".to_string(),
                    echo_prompt: false,
                    best_of: 1,
                },
                args.n_gen - 1,
                *concurrency,
                args.repetitions,
                TestName::Gen(args.n_gen),
            )?;
            results.push(r);
        }

        if args.n_prompt > 0 {
            let tks = (1000..1000 + args.n_prompt as u32).collect();
            let r = run_bench(
                mistralrs.clone(),
                RequestMessage::CompletionTokens(tks),
                1,
                *concurrency,
                args.repetitions,
                TestName::Prompt(args.n_prompt),
            )?;

            results.push(r);
        }

        print_usage(&model_name, &device, results);
    }

    Ok(())
}
