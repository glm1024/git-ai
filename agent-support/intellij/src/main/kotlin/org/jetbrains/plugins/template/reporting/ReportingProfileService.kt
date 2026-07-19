package org.jetbrains.plugins.template.reporting

import org.jetbrains.plugins.template.services.GitAiService
import java.io.ByteArrayOutputStream
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.nio.charset.StandardCharsets
import java.time.Duration
import java.util.concurrent.CompletableFuture
import java.util.concurrent.TimeUnit

class ReportingProfileService(
    private val gitAiService: GitAiService = GitAiService.getInstance(),
    private val kiloImporter: KiloReportingProfileImporter = KiloReportingProfileImporter(),
    private val cache: OrganizationOptionsCache = OrganizationOptionsCache.getInstance(),
) {
    private val httpClient = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(8)).build()

    fun loadInitialState(): InitialReportingState {
        val kiloSettings = kiloImporter.read()
        var savedSettings = ReportingSettings()
        var cliError: String? = null
        try {
            savedSettings = readGitAiSettings()
        } catch (error: Exception) {
            cliError = error.message ?: "无法读取 Git AI CLI 配置"
        }
        var settings = ReportingSupport.mergeSettings(savedSettings, kiloSettings)
        val organization = loadOrganizationOptions(settings.metricsApiBaseUrl)
        val selectedOffice = organization.options
            ?.departments?.find { it.name == settings.profile.departmentName }
            ?.offices?.find { it.name == settings.profile.officeName }
        if (selectedOffice != null && selectedOffice.teams.isEmpty()) {
            settings = settings.copy(profile = settings.profile.copy(teamName = ""))
        }
        val importedFields = ReportingSupport.importedMissingFields(savedSettings, kiloSettings)
            .filterNot { it == "组" && selectedOffice?.teams?.isEmpty() == true }
        return InitialReportingState(
            settings = settings,
            importedFields = importedFields,
            organization = organization,
            cliError = cliError,
        )
    }

    fun loadOrganizationOptions(rawUrl: String): OrganizationLoadResult {
        val endpoint = try {
            ReportingSupport.resolveOrganizationOptionsUrl(rawUrl)
        } catch (error: IllegalArgumentException) {
            return OrganizationLoadResult(error = error.message ?: "上报服务器地址无效")
        }
        return try {
            val request = HttpRequest.newBuilder(URI(endpoint))
                .timeout(Duration.ofSeconds(8))
                .header("Accept", "application/json")
                .GET()
                .build()
            val response = httpClient.send(request, HttpResponse.BodyHandlers.ofString(StandardCharsets.UTF_8))
            if (response.statusCode() !in 200..299) throw IllegalStateException("组织架构服务返回 HTTP ${response.statusCode()}")
            val options = ReportingSupport.normalizeOrganizationOptions(response.body())
            val fetchedAt = System.currentTimeMillis()
            cache.put(endpoint, fetchedAt, options)
            OrganizationLoadResult(endpoint, options, OrganizationSource.SERVER, fetchedAt)
        } catch (error: Exception) {
            val cached = cache.get(endpoint)
            val cachedOptions = cached?.takeIf { System.currentTimeMillis() - it.fetchedAt <= ORGANIZATION_CACHE_MAX_AGE_MS }
                ?.let(cache::readOptions)
            if (cached != null && cachedOptions != null) {
                OrganizationLoadResult(
                    endpoint = endpoint,
                    options = cachedOptions,
                    source = OrganizationSource.CACHE,
                    fetchedAt = cached.fetchedAt,
                    error = "${error.message ?: "组织架构服务不可用"}；正在使用上次成功加载的数据",
                )
            } else {
                OrganizationLoadResult(endpoint = endpoint, error = error.message ?: "无法加载组织架构")
            }
        }
    }

    fun save(settings: ReportingSettings): ReportingSettings {
        val normalized = ReportingSupport.normalizeSettings(settings)
        runGitAi(listOf("config", "reporting-profile", "set", "--stdin"), ReportingSupport.toCliJson(normalized))
        return readGitAiSettings()
    }

    private fun readGitAiSettings(): ReportingSettings = ReportingSupport.fromCliJson(
        runGitAi(listOf("config", "reporting-profile")),
    )

    private fun runGitAi(args: List<String>, input: String? = null): String {
        val binary = gitAiService.resolveGitAiBinary()
            ?: throw IllegalStateException("未找到 Git AI CLI，请先安装 Git AI 后重试")
        val process = ProcessBuilder(listOf(binary) + args).redirectErrorStream(false).start()
        val stdout = CompletableFuture.supplyAsync { readCapped(process.inputStream) }
        val stderr = CompletableFuture.supplyAsync { readCapped(process.errorStream) }
        if (input != null) {
            process.outputStream.bufferedWriter(StandardCharsets.UTF_8).use { it.write(input) }
        } else {
            process.outputStream.close()
        }
        if (!process.waitFor(CLI_TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
            process.destroyForcibly()
            throw IllegalStateException("Git AI CLI 响应超时，请确认 CLI 已安装后重试")
        }
        val output = stdout.get()
        val errors = stderr.get()
        if (process.exitValue() != 0) throw IllegalStateException(formatCliError(errors.ifBlank { output }))
        return output.trim()
    }

    private fun readCapped(stream: java.io.InputStream): String {
        stream.use { input ->
            val output = ByteArrayOutputStream()
            val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                if (output.size() + count > MAX_CLI_OUTPUT_BYTES) {
                    throw IllegalStateException("Git AI CLI 输出过大，已中止本次操作")
                }
                output.write(buffer, 0, count)
            }
            return output.toString(StandardCharsets.UTF_8)
        }
    }

    private fun formatCliError(message: String): String {
        if (Regex("Unknown config key:\\s*reporting[-_]profile").containsMatchIn(message)) {
            return "当前 Git AI CLI 版本不支持数据上报配置，请升级到 1.6.13 或更高版本。"
        }
        return message.ifBlank { "Git AI CLI 执行失败" }
    }

    companion object {
        private const val CLI_TIMEOUT_SECONDS = 10L
        private const val MAX_CLI_OUTPUT_BYTES = 1_000_000
        private const val ORGANIZATION_CACHE_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000L
    }
}
