# roast

A JVM starter in Rust

`roast` is a small executable that launches a JVM, similar to what using `jpackage` would output. It uses a config file to determine some options, inspired by [packr](https://github.com/libgdx/packr/) json format.

In addition to launching the JVM, it hints Windows systems with hybrid GPU setups ([NVIDIA Optimus](https://docs.nvidia.com/gameworks/content/technologies/desktop/optimus.htm), [AMD PowerXpress](https://gpuopen.com/learn/amdpowerxpressrequesthighperformance/)) to use the discrete GPU.

Roast may be launched from any Unicode-representable working directory. It initializes the JVM from
the installation directory so relative AOT classpaths remain valid, then restores the caller
directory before Java application code runs. Both the native process working directory and Java's
`user.dir` therefore match the directory from which Roast was invoked. The launcher reserves
`-Duser.dir` to enforce this contract and overrides any value supplied in `vmArgs`.

## API

`roast` will look for the following in its containing folder

### Runtime

A JDK/JRE, or a minimized image from `jlink`, in the `runtime` folder.

If you're using the macOS universal variant, the runtime folder will be `runtime-aarch64` and `runtime-x64` for aarch64 and x64 respectively. 

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
- `runOnFirstThread`: MacOS only, wether to run the JVM on the main thread, defaults to false
- `aotCache`: keeps an [AOT cache](#aot-cache) for each user so that the application starts faster, defaults to none

For example:

```json
{
  "classPath": ["app.jar"],
  "mainClass": "io.github.fourlastor.Main",
  "useZgcIfSupportedOs": true,
  "useMainAsContextClassLoader": false,
  "vmArgs": ["-Xmx1G"],
  "args": ["cli", "args"],
  "runOnFirstThread": true,
  "aotCache": {
    "id": "io.github.fourlastor.Game"
  }
}
```

### AOT cache

With `aotCache` set, roast keeps a [JEP 483](https://openjdk.org/jeps/483) AOT cache trained on real
sessions of the application. It needs a Java 25 or later runtime and jar files on the class path.

- The first launch runs in AOT record mode. When the application ends, roast starts a copy of itself
  to assemble the cache, which takes about half a second on a fast machine. If the application ends
  the process itself, with
  `System.exit` for example, the next launch assembles it before starting.
- Later launches start from the cache. A new runtime, a changed jar or different options get a new
  cache, so updating the application records once more.
- Caches live in `<cache folder>/<id>/aot`, where the cache folder is `$XDG_CACHE_HOME` or
  `~/.cache` on Linux, `~/Library/Caches` on macOS and `%LOCALAPPDATA%` on Windows. `id` defaults
  to the executable's name. The JVM's AOT messages go to a `.log` file there instead of the
  application's output.
- When the JVM refuses a cache, for example because the file is damaged, that launch runs without it
  and the next one records again. After 3 recordings without a usable cache, roast stops trying until
  the application or runtime changes.

The cache must be trained with the same garbage collector it's used with. On Java 25, ZGC
(`useZgcIfSupportedOs`) only gets part of the speedup, Java 26 and later get all of it.

Roast leaves the cache alone when `vmArgs`, `JAVA_TOOL_OPTIONS` or `_JAVA_OPTIONS` already set AOT or
CDS options, Java agents or module patching options. Set the environment variable `ROAST_AOT` to `0`,
`false`, `off` or `no` to turn the cache off, and `ROAST_AOT_CACHE_DIR` to use another folder.
