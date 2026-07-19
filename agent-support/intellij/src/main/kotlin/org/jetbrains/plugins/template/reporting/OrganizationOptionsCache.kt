package org.jetbrains.plugins.template.reporting

import com.google.gson.Gson
import com.google.gson.reflect.TypeToken
import com.intellij.openapi.components.PersistentStateComponent
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.State
import com.intellij.openapi.components.Storage
import com.intellij.openapi.components.service

@Service(Service.Level.APP)
@State(name = "GitAiReportingOrganizationCache", storages = [Storage("git-ai-reporting.xml")])
class OrganizationOptionsCache : PersistentStateComponent<OrganizationOptionsCache.CacheState> {
    data class CacheState(var entriesJson: String = "{}")
    data class Entry(val fetchedAt: Long, val optionsJson: String)

    private val gson = Gson()
    private var state = CacheState()

    override fun getState(): CacheState = state

    override fun loadState(state: CacheState) {
        this.state = state
    }

    @Synchronized
    fun get(endpoint: String): Entry? = entries()[endpoint]

    @Synchronized
    fun put(endpoint: String, fetchedAt: Long, options: OrganizationOptions) {
        val entries = entries()
        entries[endpoint] = Entry(fetchedAt, gson.toJson(options))
        state.entriesJson = gson.toJson(entries)
    }

    fun readOptions(entry: Entry): OrganizationOptions? = runCatching {
        gson.fromJson(entry.optionsJson, OrganizationOptions::class.java)
    }.getOrNull()

    private fun entries(): MutableMap<String, Entry> = runCatching {
        val type = object : TypeToken<MutableMap<String, Entry>>() {}.type
        gson.fromJson<MutableMap<String, Entry>>(state.entriesJson, type) ?: linkedMapOf()
    }.getOrElse { linkedMapOf() }

    companion object {
        fun getInstance(): OrganizationOptionsCache = service()
    }
}
