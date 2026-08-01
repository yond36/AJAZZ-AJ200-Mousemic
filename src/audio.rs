//! 音频输出: cpal/WASAPI 把解码出的 16k mono i16 PCM 输出到虚拟声卡 (如 VB-CABLE)。
//!
//! 关键点: 鼠标输出固定 16kHz, 而虚拟声卡通常只接受 44.1k/48k。cpal 不会自动重采样,
//! 故这里内置一个轻量线性重采样器 (对语音带宽足够, 无需 sinc)。输出设备选 "play" 时用
//! 系统默认输出 (扬声器试听), "cable" 时按名称匹配虚拟声卡输入 (如 "CABLE Input")。
//!
//! 输出缓冲统一用 f32: WASAPI 共享模式的默认混合格式是 float32, 用 f32 比 i16 兼容性更好,
//! 内部 PCM 队列仍为 i16 (来自 mSBC 解码), 回调里再转 f32。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 一阶线性重采样器 (带 1 样本预读), 输入率 -> 输出率。
///
/// 设计: `pos` 记录"当前输出样本对应的输入浮点位置", 每产出一个输出样本推进 `step`
/// (= in_rate/out_rate) 个输入位置; 跨过整数边界时从 `src` 拉取下一个输入样本。输入
/// 不足 (src 返回 None) 时补 0, 表现为静音, 不会因断流而崩溃。
struct LinearResampler {
    in_rate: f64,
    out_rate: f64,
    pos: f64,
    idx: u64,
    cur: f64,
    nxt: f64,
    primed: bool,
}

impl LinearResampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        LinearResampler {
            in_rate: in_rate as f64,
            out_rate: out_rate as f64,
            pos: 0.0,
            idx: 0,
            cur: 0.0,
            nxt: 0.0,
            primed: false,
        }
    }

    /// 产出 `out_frames` 个输出样本写入 `out`。
    /// `src` 在需要下一个输入样本时被调用, 返回 None 表示输入已枯竭 (补 0)。
    fn process(&mut self, out_frames: usize, src: &mut dyn FnMut() -> Option<f64>, out: &mut [f64]) {
        let step = self.in_rate / self.out_rate;
        // 预热预读样本: 否则首个输出样本恒为 0 (nxt 从 0 开始), 造成起点毛刺。
        if !self.primed {
            self.nxt = src().unwrap_or(0.0);
            self.primed = true;
        }
        for sample in out.iter_mut().take(out_frames) {
            self.pos += step;
            while self.pos >= (self.idx + 1) as f64 {
                self.idx += 1;
                self.cur = self.nxt;
                self.nxt = src().unwrap_or(0.0);
            }
            let frac = self.pos - self.idx as f64;
            *sample = self.cur * (1.0 - frac) + self.nxt * frac;
        }
    }
}

struct AudioState {
    pcm_in: Mutex<VecDeque<i16>>, // 桥接侧写入的 16k mono 输入
    resampler: Mutex<LinearResampler>,
    out_channels: usize,
    callback_count: AtomicUsize,
}

pub struct AudioOutput {
    #[allow(dead_code)]
    stream: Stream,
    state: Arc<AudioState>,
    device_name: String,
}

impl AudioOutput {
    /// 创建设备并启动输出流。
    /// `device_name`: None = 系统默认输出 (扬声器试听); Some("CABLE Input") = 匹配虚拟声卡。
    pub fn new(device_name: Option<&str>, in_rate: u32) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = if let Some(name) = device_name {
            let lower = name.to_ascii_lowercase();
            let found = host
                .output_devices()
                .map_err(|e| anyhow!("枚举输出设备失败: {}", e))?
                .find(|d| {
                    d.name()
                        .map(|n| n.to_ascii_lowercase().contains(&lower))
                        .unwrap_or(false)
                });
            match found {
                Some(d) => d,
                None => return Err(anyhow!("找不到输出设备 '{}' (用 --list 查看可用设备)", name)),
            }
        } else {
            host.default_output_device()
                .ok_or_else(|| anyhow!("没有默认输出设备"))?
        };

        let dev_name = device
            .name()
            .unwrap_or_else(|_| "<未知设备>".to_string());

        let default_cfg = device
            .default_output_config()
            .map_err(|e| anyhow!("获取设备默认配置失败: {}", e))?;
        let out_rate = default_cfg.sample_rate().0;
        let out_channels = default_cfg.channels() as usize;

        // 用 20ms 定长缓冲 (960 frames @48kHz): 比 Default 回调频繁,
        // 避免 Default 的巨量缓冲导致"填充一次后长时间静音"。
        // 480 在部分机器 WASAPI 共享模式下会建流成功但不触发回调, 960 更稳。
        let config = StreamConfig {
            channels: out_channels as u16,
            sample_rate: SampleRate(out_rate),
            buffer_size: BufferSize::Fixed(960),
        };

        let state = Arc::new(AudioState {
            pcm_in: Mutex::new(VecDeque::new()),
            resampler: Mutex::new(LinearResampler::new(in_rate, out_rate)),
            out_channels,
            callback_count: AtomicUsize::new(0),
        });
        let cb_state = state.clone();

        // 用 DEVICE 默认格式构建流: Windows/WASAPI 绝大多数为 f32, 故统一走 f32 回调。
        let stream = if default_cfg.sample_format() == cpal::SampleFormat::F32 {
            Self::build::<f32>(&device, &config, cb_state.clone())?
        } else {
            // 极少数仅支持 i16 的设备: 退回 i16 回调。
            Self::build::<i16>(&device, &config, cb_state.clone())?
        };

        stream.play().map_err(|e| anyhow!("启动输出流失败: {}", e))?;
        Ok(AudioOutput {
            stream,
            state,
            device_name: dev_name,
        })
    }

    /// 按样本类型构建输出流 (f32 / i16 两种路径, WASAPI 默认走 f32)。
    fn build<T>(
        device: &cpal::Device,
        config: &StreamConfig,
        state: Arc<AudioState>,
    ) -> anyhow::Result<Stream>
    where
        T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
    {
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                    state.callback_count.fetch_add(1, Ordering::Relaxed);
                    Self::fill::<T>(data, &state);
                },
                move |err| {
                    log::error!("音频输出流错误: {}", err);
                },
                None,
            )
            .map_err(|e| anyhow!("构建输出流失败: {} (设备可能不支持 {}Hz)", e, config.sample_rate.0))
    }

    /// cpal 回调: 从输入队列取 16k mono 样本, 重采样后写入多声道缓冲。
    fn fill<T>(data: &mut [T], state: &AudioState)
    where
        T: cpal::Sample + cpal::FromSample<f32>,
    {
        let out_channels = state.out_channels;
        let frames = data.len() / out_channels;
        let mut out_buf = vec![0.0f64; frames];

        {
            let mut pcm_in = state.pcm_in.lock().unwrap();
            let mut resampler = state.resampler.lock().unwrap();
            let pcm = &mut *pcm_in;
            let r = &mut *resampler;
            // 闭包从 i16 队列取样本并归一化到 [-1,1]; 队列空时返回 None (补静音)。
            let mut src = || pcm.pop_front().map(|s| s as f64 / 32768.0);
            r.process(frames, &mut src, &mut out_buf);
        }

        // cpal 的 Sample trait 要求归一化范围 [-1, 1]; FromSample<f32> 对 f32 是恒等,
        // 对 i16 会自动 * i16::MAX + saturating cast。这里直接传归一化值即可,
        // 不要先 * 32767 (否则 f32 路径会把远超 1 的值塞给 WASAPI, 被 clip 成满音量失真)。
        for i in 0..frames {
            let s = out_buf[i].clamp(-1.0, 1.0) as f32;
            for c in 0..out_channels {
                data[i * out_channels + c] = T::from_sample(s);
            }
        }
    }

    /// 桥接侧调用: 写入一批 16k mono i16 PCM。
    pub fn push_pcm(&self, samples: &[i16]) {
        let mut g = self.state.pcm_in.lock().unwrap();
        // 限制队列长度, 防止断流后无限堆积 (约 2 秒 @16k)
        const MAX: usize = 32000;
        for &s in samples {
            if g.len() < MAX {
                g.push_back(s);
            }
        }
    }

    /// 实际选用的输出设备名 (供 GUI/日志显示, 确认音频真的推到了预期设备)。
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// 诊断: (输入队列积压样本数, 音频回调被调用次数)。
    /// - 队列≈0 + 回调持续涨 → 消费正常(若仍无声则是设备/音量/监听问题)。
    /// - 队列涨满(≈32000) + 回调停 → 音频流卡死。
    /// - 队列在涨 + 回调快速涨 → 供给略快于消费(正常)。
    pub fn diagnostics(&self) -> (usize, usize) {
        (
            self.state.pcm_in.lock().unwrap().len(),
            self.state.callback_count.load(Ordering::Relaxed),
        )
    }
}
