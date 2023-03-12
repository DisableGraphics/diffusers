// Copyright 2022-2023 pyke.io
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{fs, path::PathBuf, sync::Arc};

use image::{DynamicImage, Rgb32FImage};
use ndarray::{concatenate, Array1, Array2, Array4, ArrayD, Axis, IxDyn};
use ndarray_einsum_beta::einsum;
use ndarray_rand::{rand_distr::StandardNormal, RandomExt};
use num_traits::ToPrimitive;
use ort::{
	tensor::{FromArray, InputTensor, OrtOwnedTensor},
	Environment, Session, SessionBuilder
};
use rand::{rngs::StdRng, Rng, SeedableRng};

use super::{StableDiffusionOptions, StableDiffusionTxt2ImgOptions};
use crate::{
	clip::CLIPStandardTokenizer,
	config::{DiffusionFramework, DiffusionPipeline, StableDiffusionConfig, TokenizerConfig},
	schedulers::DiffusionScheduler,
	Prompt, StableDiffusionCallback
};

/// A [Stable Diffusion](https://github.com/CompVis/stable-diffusion) pipeline optimized for memory usage.
///
/// This pipeline will load models only when necessary and drop them afterwards. Additionally, this pipeline **removes
/// the safety checker**, so take caution when using it (and preferably use negative prompts to prevent unsafe content
/// from being generated).
///
/// This pipeline is only intended for CPU generation in scenarios with very low available memory. Generation speed will
/// be abysmal compared to the standard [`super::StableDiffusionPipeline`], as models will be constantly loaded and
/// unloaded.
///
/// ```
/// # fn main() -> anyhow::Result<()> {
/// use pyke_diffusers::{
/// 	EulerDiscreteScheduler, OrtEnvironment, SchedulerOptimizedDefaults, StableDiffusionMemoryOptimizedPipeline,
/// 	StableDiffusionOptions, StableDiffusionTxt2ImgOptions
/// };
///
/// let environment = OrtEnvironment::default().into_arc();
/// let mut scheduler = EulerDiscreteScheduler::stable_diffusion_v1_optimized_default()?;
/// let pipeline = StableDiffusionMemoryOptimizedPipeline::new(
/// 	&environment,
/// 	"tests/stable-diffusion",
/// 	StableDiffusionOptions::default()
/// )?;
///
/// let imgs = pipeline.txt2img("photo of a red fox", &mut scheduler, StableDiffusionTxt2ImgOptions::default())?;
/// # Ok(())
/// # }
/// ```
pub struct StableDiffusionMemoryOptimizedPipeline {
	environment: Arc<Environment>,
	root: PathBuf,
	#[allow(dead_code)]
	options: StableDiffusionOptions,
	config: StableDiffusionConfig,
	tokenizer: CLIPStandardTokenizer
}

impl StableDiffusionMemoryOptimizedPipeline {
	/// Creates a new Stable Diffusion memory-optimized pipeline. This will check that the necessary models exist in
	/// `root` but will not load them until a routine is run.
	///
	/// ```
	/// # fn main() -> anyhow::Result<()> {
	/// # use pyke_diffusers::{StableDiffusionMemoryOptimizedPipeline, StableDiffusionOptions, OrtEnvironment};
	/// # let environment = OrtEnvironment::default().into_arc();
	/// let pipeline = StableDiffusionMemoryOptimizedPipeline::new(
	/// 	&environment,
	/// 	"tests/stable-diffusion",
	/// 	StableDiffusionOptions::default()
	/// )?;
	/// # Ok(())
	/// # }
	/// ```
	pub fn new(environment: &Arc<Environment>, root: impl Into<PathBuf>, options: StableDiffusionOptions) -> anyhow::Result<Self> {
		let root: PathBuf = root.into();
		let config: DiffusionPipeline = serde_json::from_reader(fs::read(root.join("diffusers.json"))?.as_slice())?;
		let config: StableDiffusionConfig = match config {
			DiffusionPipeline::StableDiffusion { framework, inner } => {
				assert_eq!(framework, DiffusionFramework::Onnx);
				inner
			}
			#[allow(unreachable_patterns)]
			_ => anyhow::bail!("not a stable diffusion pipeline")
		};

		let tokenizer = match &config.tokenizer {
			TokenizerConfig::CLIPTokenizer {
				path,
				model_max_length,
				bos_token,
				eos_token
			} => CLIPStandardTokenizer::new(root.join(path.clone()), true, *model_max_length, *bos_token, *eos_token)?,
			#[allow(unreachable_patterns)]
			_ => anyhow::bail!("not a clip tokenizer")
		};

		Ok(Self {
			environment: Arc::clone(environment),
			options,
			root,
			config,
			tokenizer
		})
	}

	fn load_text_encoder(&self) -> anyhow::Result<Session> {
		Ok(SessionBuilder::new(&self.environment)?.with_model_from_file(self.root.join(self.config.text_encoder.path.clone()))?)
	}
	fn load_vae_decoder(&self) -> anyhow::Result<Session> {
		Ok(SessionBuilder::new(&self.environment)?.with_model_from_file(self.root.join(self.config.vae.decoder.clone()))?)
	}
	fn load_unet(&self) -> anyhow::Result<Session> {
		Ok(SessionBuilder::new(&self.environment)?.with_model_from_file(self.root.join(self.config.unet.path.clone()))?)
	}

	/// Encodes the given prompt(s) into an array of text embeddings to be used as input to the UNet.
	pub fn encode_prompt(&self, prompt: Prompt, do_classifier_free_guidance: bool, negative_prompt: Option<&Prompt>) -> anyhow::Result<ArrayD<f32>> {
		let batch_size = prompt.len();
		let negative_prompt = if let Some(negative_prompt) = negative_prompt {
			if batch_size > 1 && negative_prompt.len() == 1 {
				Some(Prompt::from(vec![negative_prompt[0].clone(); batch_size]))
			} else {
				assert_eq!(batch_size, negative_prompt.len());
				Some(negative_prompt.to_owned())
			}
		} else {
			None
		};

		let text_encoder = self.load_text_encoder()?;

		let text_embeddings = if self.options.lpw {
			let embeddings = crate::pipelines::lpw::get_weighted_text_embeddings(
				&self.tokenizer,
				&text_encoder,
				prompt,
				if do_classifier_free_guidance {
					negative_prompt.or_else(|| Some(Prompt::default_batched(batch_size)))
				} else {
					negative_prompt
				},
				3,
				true
			)?;
			let mut text_embeddings = embeddings.0;
			if do_classifier_free_guidance {
				if let Some(uncond_embeddings) = embeddings.1 {
					text_embeddings = concatenate![Axis(0), uncond_embeddings, text_embeddings];
				}
			}
			text_embeddings.into_dyn()
		} else {
			let text_input_ids = self.tokenizer.encode_for_text_model(prompt.0)?.into_dyn();
			let text_embeddings = text_encoder.run(vec![InputTensor::from_array(text_input_ids)])?;
			let mut text_embeddings: ArrayD<f32> = text_embeddings[0].try_extract()?.view().to_owned();

			if do_classifier_free_guidance {
				let uncond_input = self
					.tokenizer
					.encode_for_text_model(negative_prompt.unwrap_or_else(|| Prompt::default_batched(batch_size)).0)?
					.into_dyn();
				let uncond_embeddings = text_encoder.run(vec![InputTensor::from_array(uncond_input)])?;
				let uncond_embeddings: ArrayD<f32> = uncond_embeddings[0].try_extract()?.view().to_owned();
				text_embeddings = concatenate![Axis(0), uncond_embeddings, text_embeddings];
			}

			text_embeddings
		};

		Ok(text_embeddings)
	}

	fn to_image(&self, width: u32, height: u32, arr: &Array4<f32>) -> anyhow::Result<DynamicImage> {
		Ok(DynamicImage::ImageRgb32F(
			Rgb32FImage::from_raw(width, height, arr.map(|f| f.clamp(0.0, 1.0)).into_iter().collect::<Vec<_>>())
				.ok_or_else(|| anyhow::anyhow!("failed to construct image"))?
		))
	}

	/// Decodes UNet latents via a cheap approximation into an array of [`image::DynamicImage`]s.
	pub fn approximate_decode_latents(&self, latents: Array4<f32>) -> anyhow::Result<Vec<DynamicImage>> {
		let coefs = Array2::from_shape_vec((4, 3), vec![0.298, 0.207, 0.208, 0.187, 0.286, 0.173, -0.158, 0.189, 0.264, -0.184, -0.271, -0.473])?;
		let approx = einsum("blxy,lr->bxyr", &[&latents, &coefs]).expect("einsum error");
		let mut images = Vec::new();
		for approx_chunk in approx.axis_iter(Axis(0)) {
			let approx_chunk = approx_chunk.insert_axis(Axis(0)).into_dimensionality()?;
			let approx_chunk = approx_chunk.to_owned() * 1.2;
			let image = self.to_image(approx_chunk.shape()[1] as _, approx_chunk.shape()[2] as _, &approx_chunk)?;
			images.push(image);
		}
		Ok(images)
	}

	/// Decodes UNet latents via the variational autoencoder into an array of [`image::DynamicImage`]s.
	pub fn decode_latents(&self, mut latents: Array4<f32>) -> anyhow::Result<Vec<DynamicImage>> {
		latents = 1.0 / 0.18215 * latents;

		let vae_decoder = self.load_vae_decoder()?;
		let latent_vae_input: ArrayD<f32> = latents.into_dyn();
		let mut images = Vec::new();
		for latent_chunk in latent_vae_input.axis_iter(Axis(0)) {
			let latent_chunk = latent_chunk.to_owned().insert_axis(Axis(0));
			let image = vae_decoder.run(vec![InputTensor::from_array(latent_chunk)])?;
			let image: OrtOwnedTensor<'_, f32, IxDyn> = image[0].try_extract()?;
			let f_image: Array4<f32> = image.view().to_owned().into_dimensionality()?;
			let f_image = f_image.permuted_axes([0, 2, 3, 1]).map(|f| (f / 2.0 + 0.5).clamp(0.0, 1.0));

			let image = self.to_image(f_image.shape()[1] as _, f_image.shape()[2] as _, &f_image)?;
			images.push(image);
		}

		Ok(images)
	}

	/// Generates images from given text prompt(s). Returns a vector of [`image::DynamicImage`]s, using float32 buffers.
	/// In most cases, you'll want to convert the images into RGB8 via `img.clone().into_rgb8().`
	///
	/// `scheduler` must be a Stable Diffusion-compatible scheduler.
	///
	/// See [`StableDiffusionTxt2ImgOptions`] for additional configuration.
	///
	/// # Examples
	///
	/// Simple text-to-image:
	/// ```
	/// # fn main() -> anyhow::Result<()> {
	/// # use pyke_diffusers::{EulerDiscreteScheduler, StableDiffusionMemoryOptimizedPipeline, StableDiffusionOptions, StableDiffusionTxt2ImgOptions, OrtEnvironment};
	/// # let environment = OrtEnvironment::default().into_arc();
	/// # let mut scheduler = EulerDiscreteScheduler::default();
	/// let pipeline =
	/// 	StableDiffusionMemoryOptimizedPipeline::new(&environment, "tests/stable-diffusion", StableDiffusionOptions::default())?;
	///
	/// let imgs = pipeline.txt2img("photo of a red fox", &mut scheduler, StableDiffusionTxt2ImgOptions::default())?;
	/// imgs[0].clone().into_rgb8().save("result.png")?;
	/// # Ok(())
	/// # }
	/// ```
	pub fn txt2img<S: DiffusionScheduler>(
		&self,
		prompt: impl Into<Prompt>,
		scheduler: &mut S,
		options: StableDiffusionTxt2ImgOptions
	) -> anyhow::Result<Vec<DynamicImage>> {
		let steps = options.steps;

		let seed = options.seed.unwrap_or_else(|| rand::thread_rng().gen::<u64>());
		let mut rng = StdRng::seed_from_u64(seed);

		if options.height % 8 != 0 || options.width % 8 != 0 {
			anyhow::bail!("`width` ({}) and `height` ({}) must be divisible by 8 for Stable Diffusion", options.width, options.height);
		}

		let prompt: Prompt = prompt.into();
		let batch_size = prompt.len();

		let do_classifier_free_guidance = options.guidance_scale > 1.0;
		let text_embeddings = self.encode_prompt(prompt, do_classifier_free_guidance, options.negative_prompt.as_ref())?;

		let latents_shape = (batch_size, 4_usize, (options.height / 8) as usize, (options.width / 8) as usize);
		let mut latents = Array4::<f32>::random_using(latents_shape, StandardNormal, &mut rng);

		scheduler.set_timesteps(steps);
		latents *= scheduler.init_noise_sigma();

		let timesteps = scheduler.timesteps().to_owned();
		let num_warmup_steps = timesteps.len() - options.steps * S::order();

		{
			let unet = self.load_unet()?;

			for (i, t) in timesteps.to_owned().indexed_iter() {
				let latent_model_input = if do_classifier_free_guidance {
					concatenate![Axis(0), latents, latents]
				} else {
					latents.to_owned()
				};
				let latent_model_input = scheduler.scale_model_input(latent_model_input.view(), *t);

				let latent_model_input: ArrayD<f32> = latent_model_input.into_dyn();
				let timestep: ArrayD<f32> = Array1::from_iter([t.to_f32().unwrap()]).into_dyn();
				let encoder_hidden_states: ArrayD<f32> = text_embeddings.clone().into_dyn();

				let noise_pred = unet.run(vec![
					InputTensor::from_array(latent_model_input),
					InputTensor::from_array(timestep),
					InputTensor::from_array(encoder_hidden_states),
				])?;
				let noise_pred: OrtOwnedTensor<'_, f32, IxDyn> = noise_pred[0].try_extract()?;
				let noise_pred: Array4<f32> = noise_pred.view().to_owned().into_dimensionality()?;

				let mut noise_pred: Array4<f32> = noise_pred.clone();
				if do_classifier_free_guidance {
					let mut noise_pred_chunks = noise_pred.axis_iter(Axis(0));
					let (noise_pred_uncond, noise_pred_text) = (noise_pred_chunks.next().unwrap(), noise_pred_chunks.next().unwrap());
					let (noise_pred_uncond, noise_pred_text) =
						(noise_pred_uncond.insert_axis(Axis(0)).to_owned(), noise_pred_text.insert_axis(Axis(0)).to_owned());
					noise_pred = &noise_pred_uncond + options.guidance_scale * (noise_pred_text - &noise_pred_uncond);
				}

				let scheduler_output = scheduler.step(noise_pred.view(), *t, latents.view(), &mut rng);
				latents = scheduler_output.prev_sample().to_owned();

				if let Some(callback) = options.callback.as_ref() {
					if i == timesteps.len() - 1 || ((i + 1) > num_warmup_steps && (i + 1) % S::order() == 0) {
						let keep_going = match callback {
							StableDiffusionCallback::Progress { frequency, cb } if i % frequency == 0 => cb(i, t.to_f32().unwrap()),
							StableDiffusionCallback::Latents { frequency, cb } if i % frequency == 0 => cb(i, t.to_f32().unwrap(), latents.clone()),
							StableDiffusionCallback::Decoded { frequency, cb } if i != 0 && i % frequency == 0 => {
								cb(i, t.to_f32().unwrap(), self.decode_latents(latents.clone())?)
							}
							StableDiffusionCallback::ApproximateDecoded { frequency, cb } if i != 0 && i % frequency == 0 => {
								cb(i, t.to_f32().unwrap(), self.approximate_decode_latents(latents.clone())?)
							}
							_ => true
						};
						if !keep_going {
							break;
						}
					}
				}
			}

			std::mem::drop(unet);
		}

		self.decode_latents(latents)
	}
}
