# roast

A JVM starter in Rust

`roast` is a small executable that launches a JVM, similar to what using `jpackage` would output. It uses a config file to determine some options, inspired by [packr](https://github.com/libgdx/packr/) json format.

In addition to launching the JVM, it hints Windows systems with hybrid GPU setups ([NVIDIA Optimus](https://docs.nvidia.com/gameworks/content/technologies/desktop/optimus.htm), [AMD PowerXpress](https://gpuopen.com/learn/amdpowerxpressrequesthighperformance/)) to use the discrete GPU.

## API

`roast` will look for the following in its containing folder

### Runtime

A JDK/JRE, or a minimized image from `jlink`, in the `runtime` folder.

### JSON config file

The config file must be in the `app` folder, and must have the same name as the executable, so for example, if your executable is `game` (or `game.exe`), the config file will be `app/game.json`.

It's possible to have multiple copies of roast named differently to support different launch options on the same runtime. For example, you could have `game` and `game-debug` executables corresponding to `app/game.json` and `app/game-debug.json`.

The config file supports the following options:

- `classPath`: path of the jars to include in the application classpath (usually, your application jar), this is mandatory
- `mainClass`: the package and name of the class from which to call the method `public static void main(String[] args)`, this is mandatory
- `useZgcIfSupportedOs`: uses ZGC when [supported](https://wiki.openjdk.org/display/zgc/Main#Main-SupportedPlatforms), defaults to false
- `useMainAsContextClassLoader`: sets the main class as the main thread's [context class loader](https://docs.oracle.com/javase/8/docs/api/java/lang/Thread.html#getContextClassLoader--), defaults to false
- `vmArgs`: arguments to pass to the java runtime, defaults to an empty array
- `args`: arguments to the pass to main method, defaults to an empty array

For example:

```json
{
  "classPath": ["app.jar"],
  "mainClass": "io.github.fourlastor.Main",
  "useZgcIfSupportedOs": true,
  "useMainAsContextClassLoader": false,
  "vmArgs": ["-Xmx1G"],
  "args": ["cli", "args"]
}
```
