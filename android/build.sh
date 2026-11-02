#!/usr/bin/env bash
# Builds out/ionhf.apk (debug-signed; ABIS="arm64-v8a x86_64" adds the
# emulator's) with the SDK's own tools: no
# Gradle. Needs JDK 17+, the Android SDK (platforms;android-36,
# build-tools;36.0.0, ndk) and cargo-ndk.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/.."
sdk="${ANDROID_HOME:-$LOCALAPPDATA/Android/Sdk}"
sdk="$(cygpath -u "$sdk" 2>/dev/null || echo "$sdk")"
ndk="${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* | sort -V | tail -1)}"
bt="$sdk/build-tools/36.0.0"
jar="$sdk/platforms/android-36/android.jar"
if [ -z "${JAVA_HOME:-}" ]; then
    JAVA_HOME="$(ls -d "$LOCALAPPDATA"/Programs/jdk-* 2>/dev/null | sort -V | tail -1)"
fi
export JAVA_HOME ANDROID_HOME="$sdk" ANDROID_NDK_HOME="$ndk"
target="${CARGO_TARGET_DIR:-$root/target}"
out="$here/out"
rm -rf "$out" && mkdir -p "$out/res/mipmap-xxxhdpi"

for abi in ${ABIS:-arm64-v8a}; do
    case $abi in
        arm64-v8a) triple=aarch64-linux-android ;;
        x86_64) triple=x86_64-linux-android ;;
        *) echo "unknown ABI $abi" >&2; exit 1 ;;
    esac
    (cd "$root" && cargo ndk -t "$abi" -P 26 build --release -j "${JOBS:-2}" -p ionhf-android)
    mkdir -p "$out/lib/$abi"
    cp "$target/$triple/release/libionhf_android.so" "$out/lib/$abi/"
done

cp -r "$here/res/." "$out/res/"
cp "$root/crates/ionhf/assets/icon.png" "$out/res/mipmap-xxxhdpi/ic_launcher.png"
"$bt/aapt2" compile --dir "$out/res" -o "$out/res.zip"
"$bt/aapt2" link -o "$out/base.apk" -I "$jar" --manifest "$here/AndroidManifest.xml" "$out/res.zip"

mkdir -p "$out/classes" "$out/dex"
"$JAVA_HOME/bin/javac" -nowarn -Xlint:-options -source 8 -target 8 -bootclasspath "$jar" -d "$out/classes"     $(find "$here/java" -name '*.java')
"$bt/d8.bat" --release --min-api 26 --lib "$jar" --output "$out/dex" $(find "$out/classes" -name '*.class') 2>/dev/null     || "$bt/d8" --release --min-api 26 --lib "$jar" --output "$out/dex" $(find "$out/classes" -name '*.class')

python -I - "$out/base.apk" "$out/lib" "$out/dex/classes.dex" <<'PY'
import os, sys, zipfile
apk, lib, dex = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    z.write(dex, "classes.dex")
    for d, _, files in os.walk(lib):
        for f in files:
            p = os.path.join(d, f)
            z.write(p, "lib/" + os.path.relpath(p, lib).replace(os.sep, "/"))
PY
"$bt/zipalign" -f -p 4 "$out/base.apk" "$out/aligned.apk"

ks="$HOME/.android/debug.keystore"
if [ ! -f "$ks" ]; then
    mkdir -p "$(dirname "$ks")"
    "$JAVA_HOME/bin/keytool" -genkeypair -keystore "$ks" -storepass android -keypass android \
        -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
fi
"$bt/apksigner.bat" sign --ks "$ks" --ks-pass pass:android --out "$out/ionhf.apk" "$out/aligned.apk" 2>/dev/null \
    || "$bt/apksigner" sign --ks "$ks" --ks-pass pass:android --out "$out/ionhf.apk" "$out/aligned.apk"
echo "$out/ionhf.apk"
