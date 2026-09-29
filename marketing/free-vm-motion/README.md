# GAP Free MicroVM motion design

The 30-second, silent 1280×720 film on `/free-vm` is rendered from the Remotion
composition in `src/motion.tsx`. The production MP4, WebM and poster are checked in
under `src/ui/` and served by the GAP node itself.

To rebuild after changing the composition, run `npm ci` and `npm run render` in
this directory, then regenerate the poster:

```sh
ffmpeg -y -ss 2 -i ../../src/ui/free-vm-motion.mp4 -frames:v 1 ../../src/ui/free-vm-motion-poster.webp
ffmpeg -y -i ../../src/ui/free-vm-motion.mp4 -an -c:v libvpx-vp9 -b:v 0 -crf 36 -row-mt 1 -deadline good -cpu-used 3 ../../src/ui/free-vm-motion.webm
```

The composition is exactly 900 frames at 30 fps. It uses no generated voice or
third-party imagery, so the product text remains editable and the film has no
external media dependency at runtime.
