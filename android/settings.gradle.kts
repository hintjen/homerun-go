pluginManagement {
    repositories {
        google {
            content {
                includeGroupByRegex("com\\.android.*")
                includeGroupByRegex("com\\.google.*")
                includeGroupByRegex("androidx.*")
            }
        }
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "Homerun"
include(":app")
// Every Java runtime is a Play feature module, one each. They do not all use
// the same delivery: 25 is packaged with the app, 17 is always downloaded, and
// 21 is between the two for now. See docs/android-server-backend.md.
include(":jre17")
include(":jre21")
include(":jre25")
