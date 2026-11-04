# JSL compute research

Whether JSL can drive high-performance numerical computation, and on what architecture. This
directory is isolated: no public L3i API, default feature or dependency depends on it.

- [`findings.md`](findings.md): the architectural decision, the questions it answers, measurements,
  the CubeCL verdict and what remains. Start here.
- [`graph-cpu/`](graph-cpu/README.md): the implementation and its contract.
- [`results/`](results/): raw reports. `compute-*.txt` are the current benchmark runs,
  `graph-cpu-*.txt` the original interpreter's, `mutations.txt` the injected-bug record,
  `kernel-instructions.txt` and `luau-relu-sum-ir-and-x64.txt` the compiled-code evidence.
- [`fused-shader.jsl`](fused-shader.jsl): a handwritten Jess compute shader over persistent buffers.
- [`verify.sh`](verify.sh): every check, from the repository root.

The CubeCL experiments were removed once measured and rejected. Their last revision is `9e70c0f`, under
`research/cubecl-cpu` and `research/cubecl-cpu-010`.
