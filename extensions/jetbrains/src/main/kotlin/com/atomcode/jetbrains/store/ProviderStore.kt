package com.atomcode.jetbrains.store

import com.atomcode.jetbrains.client.DaemonClient
import com.atomcode.jetbrains.protocol.*
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

class ProviderStore(
    private val client: DaemonClient,
    private val scope: CoroutineScope
) {
    private val _providers = MutableStateFlow<List<ProviderInfo>>(emptyList())
    val providers: StateFlow<List<ProviderInfo>> = _providers.asStateFlow()

    private val _models = MutableStateFlow<List<ModelInfo>>(emptyList())
    val models: StateFlow<List<ModelInfo>> = _models.asStateFlow()

    private val _defaultProvider = MutableStateFlow<String?>(null)
    val defaultProvider: StateFlow<String?> = _defaultProvider.asStateFlow()

    fun refresh() {
        scope.launch {
            try {
                _providers.value = client.listProviders()
                _models.value = client.listModels()
                _defaultProvider.value = _providers.value.firstOrNull { it.isDefault }?.name
            } catch (_: Exception) {}
        }
    }

    suspend fun createProvider(request: CreateProviderRequest) {
        client.createProvider(request)
        refresh()
    }

    suspend fun patchProvider(name: String, request: PatchProviderRequest) {
        client.patchProvider(name, request)
        refresh()
    }

    suspend fun deleteProvider(name: String) {
        client.deleteProvider(name)
        refresh()
    }

    suspend fun setDefaultProvider(name: String) {
        client.setDefaultProvider(name)
        refresh()
    }

    suspend fun patchThinking(name: String, request: PatchThinkingRequest) {
        client.patchThinking(name, request)
        refresh()
    }
}
