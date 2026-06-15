package com.atomcode.jetbrains.security

import kotlin.test.Test
import kotlin.test.assertEquals

class SensitivePathClassifierTest {
    @Test
    fun blocksPrivateKeys() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/id_ed25519"))
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/cert.pem"))
    }

    @Test
    fun blocksPemExtension() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/cert.pem"))
    }

    @Test
    fun blocksKeyExtension() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/private.key"))
    }

    @Test
    fun blocksP12Extension() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/cert.p12"))
    }

    @Test
    fun blocksPfxExtension() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/cert.pfx"))
    }

    @Test
    fun blocksJksExtension() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/keystore.jks"))
    }

    @Test
    fun blocksSshDirectory() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/home/user/.ssh/authorized_keys"))
    }

    @Test
    fun blocksAwsDirectory() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/home/user/.aws/credentials"))
    }

    @Test
    fun blocksGnupgDirectory() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/home/user/.gnupg/gpg.conf"))
    }

    @Test
    fun blocksIdRsaByName() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/id_rsa"))
    }

    @Test
    fun blocksDotNetrcByName() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/home/user/.netrc"))
    }

    @Test
    fun blocksCredentialsByName() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/credentials"))
    }

    @Test
    fun blocksKubeconfigByName() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/kubeconfig"))
    }

    @Test
    fun stronglyConfirmsDotEnv() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.env"))
    }

    @Test
    fun stronglyConfirmsDotEnvLocal() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.env.local"))
    }

    @Test
    fun stronglyConfirmsDotEnvProduction() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.env.production"))
    }

    @Test
    fun stronglyConfirmsDotNpmrc() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.npmrc"))
    }

    @Test
    fun stronglyConfirmsDotPypirc() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.pypirc"))
    }

    @Test
    fun stronglyConfirmsSettingsXml() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/settings.xml"))
    }

    @Test
    fun stronglyConfirmsTerraformDirectory() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.terraform/state"))
    }

    @Test
    fun stronglyConfirmsTfstateFile() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/terraform.tfstate"))
    }

    @Test
    fun stronglyConfirmsGitDirectory() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.git/config"))
    }

    @Test
    fun warnsLogFiles() {
        assertEquals(PathSensitivity.Warn, SensitivePathClassifier.classify("/repo/app.log"))
    }

    @Test
    fun warnsDumpFiles() {
        assertEquals(PathSensitivity.Warn, SensitivePathClassifier.classify("/repo/database.dump"))
    }

    @Test
    fun warnsBakFiles() {
        assertEquals(PathSensitivity.Warn, SensitivePathClassifier.classify("/repo/backup.bak"))
    }

    @Test
    fun normalForRegularSourceFile() {
        assertEquals(PathSensitivity.Normal, SensitivePathClassifier.classify("/repo/src/main.kt"))
    }

    @Test
    fun normalForEmptyPath() {
        assertEquals(PathSensitivity.Normal, SensitivePathClassifier.classify(""))
    }

    @Test
    fun normalizesWindowsBackslashes() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("C:\\Users\\me\\.ssh\\id_ed25519"))
    }

    @Test
    fun blocksCaseInsensitive() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/MY.PEM"))
    }

    @Test
    fun warnsCaseInsensitive() {
        assertEquals(PathSensitivity.Warn, SensitivePathClassifier.classify("/repo/ERROR.LOG"))
    }

    @Test
    fun `blocks id_rsa case-insensitively`() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/ID_RSA"))
    }

    @Test
    fun `blocks kubeconfig case-insensitively`() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/KUBECONFIG"))
    }

    @Test
    fun `blocks strongConfirm env files case-insensitively`() {
        assertEquals(PathSensitivity.StrongConfirm, SensitivePathClassifier.classify("/repo/.ENV"))
    }

    @Test
    fun `blocks credentials case-insensitively`() {
        assertEquals(PathSensitivity.Block, SensitivePathClassifier.classify("/repo/CREDENTIALS"))
    }
}

