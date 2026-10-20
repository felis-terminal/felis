# felis-render-wgpu

The wgpu renderer of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for persistent
sessions and headless control: the pipeline, instance encoding, the glyph and image atlas textures and the decoration
shader.

It draws to a surface built from the raw window handle the caller supplies.
