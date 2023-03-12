# Copyright 2022-2023 pyke.io
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# 	http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

from pathlib import Path
import sys
from typing import List, Mapping, Sequence, Tuple, Union

import onnx
import torch

from _utils import collect_garbage

@torch.inference_mode()
def onnx_export(
	model: torch.nn.Module,
	model_args: Tuple,
	output_path: Path,
	ordered_input_names: List[str],
	output_names: List[str],
	dynamic_axes: Union[Mapping[str, Mapping[int, str]], Mapping[str, Sequence[int]]],
	opset: int = 15
):
	torch.onnx.export(
		model,
		model_args,
		f=output_path.as_posix(),
		input_names=ordered_input_names,
		output_names=output_names,
		dynamic_axes=dynamic_axes,
		do_constant_folding=True,
		opset_version=opset
	)

def onnx_simplify(model_path: Path):	
	from onnxsim import simplify

	model = onnx.load(str(model_path))
	model_opt, check = simplify(model)
	if not check:
		print(f"failed to validate simplified model at {model_path}")
		sys.exit(1)

	del model
	onnx.save(model_opt, str(model_path))
	del model_opt
	collect_garbage()
