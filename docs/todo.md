
- Single decode worker is fine in practice (browser parallelizes createImageBitmap), but a small
  worker pool would eliminate the JS orchestration bottleneck under burst load.

- Add virtual folder view, default hierarchy should be plugin name and user folders at the top and a folder for each tag inside. The folder view is entirely virtual and does not actually copy or move images.
