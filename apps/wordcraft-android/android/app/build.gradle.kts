plugins {
    id("com.android.application")
}

android {
    namespace = "ai.storyteller.wordcraft"
    compileSdk = 35

    defaultConfig {
        applicationId = "ai.storyteller.wordcraft"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.18.1"
        ndk {
            // Same ABIs as before: everything still shipping, and no x86 emulator
            // build to double the APK.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
        }
        release {
            isMinifyEnabled = false
            // Installable from CI before a signing key is set up. Replace with a
            // real signing config before publishing anywhere.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // `cargo ndk -o app/src/main/jniLibs` puts libwordcraft_android.so here.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}
