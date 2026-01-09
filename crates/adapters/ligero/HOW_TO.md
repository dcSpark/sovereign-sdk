
## Running the Prover/Verifier

To run the the prover follow these steps:

### prover
* Navigate to the build directory and run the following command to run the prover:

``` bash
./webgpu_prover <string of JSON object>
```
where the single argument is a string produced by `JSON.stringify()`. Here is the fields of the JSON:

|       Field       | Type     | Required |   Default  |          Description         |
| ----------------- | -------- | -------- | ---------- | ---------------------------- |
| `program`         | string   | &check;  |            | Path to the application wasm |
| `gpu-threads`     | int      |          | packing    | Number of GPU threads to use (can be more than physical cores) |
| `shader-path`     | string   |          | "./shader" | Path to the folder contains GPU shaders |
| `packing`         | int      |          | 8192       | FFT message packing size (doubled for codeword) |
| `private-indices` | [int]    |          | []         | Index of private arguments. Start from 1 |
| `args`            | [object] |          | []         | Program arguments. Objects are in the form of { <type> : <val> } where `type` is `str`/`i64`/`hex` |

**Note**: Packing size influences the proof length. This parameter needs to be optimally chosen to minimize proof length.

### verifier
* Navigate to the build directory and run the following command to run the verifier:

``` bash
./webgpu_verifier <equivalent JSON argument as for demo, but with obscured private indices>
```
## Examples

**Note:** When in doubt, it's always a good idea to recompile the example again from source in `/examples`. For example, the latest interface takes a JSON as input which contains more information than the old interface. As a consequence, we no longer need to manually convert the input from string to `int` or raw hex, all we need now is a simple `reinterpret_cast`. The old application still works by taking all input as string but it's less efficient.

Navigate to the `build` directory to run the examples.

### Example 1: Edit Distance
Suppose we have `edit.wasm` either by compiling `/examples/edit_distance.cpp` or pick from `wasm/edit.wasm`. The arguments are `abcde` and `bcdef`, then:

```bash
./webgpu_prover '{"program":"../sdk/build/examples/edit.wasm","shader-path":"../shader","packing":8192,"private-indices":[1],"args":[{"str":"abcdeabcdeabcde"},{"str":"bcdefabcdeabcde"},{"i64":15},{"i64":15}]}'
```

This will run the Edit Distance program with the given packing size and arguments and verify that the edit distance between the two input strings `abcde` and `bcdef` is less than 5. The last two arguments are the length of the two input strings. You can run this code with two strings of arbitrary lengths. If you try to generate a proof with strings whose edit distance >= 5, the verification will fail.

For verification "private" arguments can be "obscured" as long as their input type is still correct and the argument length is the same:

```bash
./webgpu_verifier '{"program":"../sdk/build/examples/edit.wasm","shader-path":"../shader","packing":8192,"private-indices":[1],"args":[{"str":"xxxxxxxxxxxxxxx"},{"str":"bcdefabcdeabcde"},{"i64":15},{"i64":15}]}'
```

## Hardware Acceleration

Ligetron uses WebGPU (through Dawn or Emscripten) to accelerate the computation both natively and on browsers. Currently, we don't provide a fallback implementation if WebGPU is not available on your system. Technically, every device with the support of `DX12/Vulkan/Metal` should be able to run with the exception of iOS devices (specifically, iPhones).

On Linux, you might need additional flags to enable WebGPU support on browsers:

```bash
google-chrome --enable-unsafe-webgpu --enable-features=Vulkan
```

------

Copyright (C) 2023-2025 Ligero, Inc