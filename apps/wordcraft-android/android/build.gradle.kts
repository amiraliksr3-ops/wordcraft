// The Android app is assembled by Gradle: the Rust library is built with
// `cargo ndk` into `app/src/main/jniLibs` first, and this packages it with the
// activity, manifest and resources.
plugins {
    id("com.android.application") version "8.10.1" apply false
}
