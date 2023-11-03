# roast

A JVM starter in Rust

`roast` is a small executable that launches a JVM, similar to what using `jpackage` would output. It uses a config file to determine some options, following [packr](https://github.com/libgdx/packr/) json format:

```json
{
  "classPath": [
    "my-game.jar"
  ],
  "mainClass": "io.github.fourlastor.gdx.lwjgl3.Lwjgl3Launcher",
  "useZgcIfSupportedOs": true,
  "vmArgs": [
    "-Xmx1G"
  ]
}
```

In addition to launching the JVM, it hints Windows systems with hybrid GPU setups ([NVIDIA Optimus](https://docs.nvidia.com/gameworks/content/technologies/desktop/optimus.htm), [AMD PowerXpress](https://gpuopen.com/learn/amdpowerxpressrequesthighperformance/)) to use the discrete GPU.


## API

`roast` will look for the following in its containing folder:

1. Config file `config.json` and the referenced jars.
1. A JDK/JRE (or a minimized image from `jlink`) in a folder called `jdk`

