# Release media inputs

`media-sources.sha256` pins the source archives used by the Linux runtime builder.
Update it together with the versions in `build_release_gstreamer.sh` and the
Windows workflow, then repeat the release qualification checks.

`smoke.h264` and `smoke.h265` contain two generated 64×64 grey video frames.
They have no camera/user content. The release smoke check decodes both with the
bundled FFmpeg plugins, in addition to generating/decoding JPEG through GStreamer.
These fixtures can be recreated with FFmpeg:

```bash
ffmpeg -f lavfi -i color=c=gray:s=64x64:r=5 -frames:v 2 \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -bsf:v filter_units=remove_types=6 -f h264 smoke.h264
ffmpeg -f lavfi -i color=c=gray:s=64x64:r=5 -frames:v 2 \
  -c:v libx265 -preset ultrafast -pix_fmt yuv420p \
  -x265-params pools=1:frame-threads=1:log-level=error \
  -bsf:v 'filter_units=remove_types=39|40' -f hevc smoke.h265
```

H.264/H.265 encoders are used only to create these fixtures; they are not
dependencies of the release build or part of the bundled Linux FFmpeg build.
