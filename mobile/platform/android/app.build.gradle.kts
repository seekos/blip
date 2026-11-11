plugins {
    id("com.android.application")
}

val releaseStoreFile = System.getenv("BLIP_ANDROID_KEYSTORE")
val releaseKeyAlias = System.getenv("BLIP_ANDROID_KEY_ALIAS")
val releaseStorePassword = System.getenv("BLIP_ANDROID_STORE_PASSWORD")
val releaseKeyPassword = System.getenv("BLIP_ANDROID_KEY_PASSWORD") ?: releaseStorePassword
val releaseSigningConfigured = listOf(
    releaseStoreFile,
    releaseKeyAlias,
    releaseStorePassword,
).all { !it.isNullOrBlank() }

android {
    namespace = "com.amov.blip.mobile"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.amov.blip.mobile"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a") }
        manifestPlaceholders["nativeLibraryName"] = "blip_mobile"
    }

    signingConfigs {
        if (releaseSigningConfigured) {
            create("release") {
                storeFile = file(releaseStoreFile!!)
                storePassword = releaseStorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName(
                if (releaseSigningConfigured) "release" else "debug",
            )
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
        debug {
            isDebuggable = true
            isJniDebuggable = true
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging.jniLibs.keepDebugSymbols += "*/arm64-v8a/libblip_mobile.so"
    lint {
        abortOnError = false
        checkReleaseBuilds = false
    }
}

dependencies {
    implementation("androidx.core:core:1.12.0")
    implementation("androidx.core:core-splashscreen:1.0.1")
    implementation("androidx.biometric:biometric:1.1.0")
    implementation("androidx.media:media:1.7.1")
}

