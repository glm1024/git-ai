package org.jetbrains.plugins.template.reporting

import com.google.gson.Gson
import com.google.gson.JsonElement
import com.google.gson.JsonObject
import com.google.gson.JsonParser
import java.net.URI
import java.util.LinkedHashSet

object ReportingSupport {
    const val DEFAULT_METRICS_API_BASE_URL = "http://100.7.132.102:8081/prod-api"

    private const val ORGANIZATION_OPTIONS_PATH = "/api/v1/ai-code-stats/organization-options"
    private val knownEndpointPaths = listOf(
        "/worker/metrics/upload",
        ORGANIZATION_OPTIONS_PATH,
        "/api/v1/ingest/ai-code-stats",
        "/api/v1/ingest/ai-token-usage",
    )
    private val gson = Gson()

    fun normalizeSettings(settings: ReportingSettings): ReportingSettings = ReportingSettings(
        metricsApiBaseUrl = normalizeMetricsApiBaseUrl(settings.metricsApiBaseUrl),
        profile = ReportingProfile(
            departmentName = settings.profile.departmentName.trim(),
            officeName = settings.profile.officeName.trim(),
            teamName = settings.profile.teamName.trim(),
            userName = settings.profile.userName.trim(),
            userEmail = settings.profile.userEmail.trim().lowercase(),
        ),
    )

    fun mergeSettings(saved: ReportingSettings, kilo: ReportingSettings): ReportingSettings {
        val normalizedSaved = normalizeForMerge(saved, preserveInvalidAddress = true)
        val normalizedKilo = normalizeForMerge(kilo, preserveInvalidAddress = false)
        fun choose(current: String, imported: String): String = current.ifEmpty { imported }
        return ReportingSettings(
            metricsApiBaseUrl = choose(
                normalizedSaved.metricsApiBaseUrl,
                normalizedKilo.metricsApiBaseUrl.ifEmpty { DEFAULT_METRICS_API_BASE_URL },
            ),
            profile = ReportingProfile(
                departmentName = choose(normalizedSaved.profile.departmentName, normalizedKilo.profile.departmentName),
                officeName = choose(normalizedSaved.profile.officeName, normalizedKilo.profile.officeName),
                teamName = choose(normalizedSaved.profile.teamName, normalizedKilo.profile.teamName),
                userName = choose(normalizedSaved.profile.userName, normalizedKilo.profile.userName),
                userEmail = choose(normalizedSaved.profile.userEmail, normalizedKilo.profile.userEmail),
            ),
        )
    }

    fun importedMissingFields(saved: ReportingSettings, kilo: ReportingSettings): List<String> = listOf(
        Triple("上报服务器地址", saved.metricsApiBaseUrl, kilo.metricsApiBaseUrl),
        Triple("部门", saved.profile.departmentName, kilo.profile.departmentName),
        Triple("处", saved.profile.officeName, kilo.profile.officeName),
        Triple("组", saved.profile.teamName, kilo.profile.teamName),
        Triple("姓名", saved.profile.userName, kilo.profile.userName),
        Triple("公司邮箱", saved.profile.userEmail, kilo.profile.userEmail),
    ).filter { (_, current, imported) -> current.isBlank() && imported.isNotBlank() }.map { it.first }

    fun normalizeMetricsApiBaseUrl(rawValue: String): String {
        val trimmed = rawValue.trim()
        if (trimmed.isEmpty()) return ""
        val uri = try {
            URI(trimmed)
        } catch (_: Exception) {
            throw IllegalArgumentException("上报服务器地址无效")
        }
        if (uri.scheme !in setOf("http", "https") || uri.host.isNullOrBlank()) {
            throw IllegalArgumentException("上报服务器地址必须以 http:// 或 https:// 开头")
        }
        if (!uri.userInfo.isNullOrEmpty() || !uri.query.isNullOrEmpty() || !uri.fragment.isNullOrEmpty()) {
            throw IllegalArgumentException("上报服务器地址不能包含账号、查询参数或片段")
        }
        var path = uri.path.orEmpty().replace(Regex("/+${'$'}"), "").ifEmpty { "/" }
        knownEndpointPaths.firstOrNull { path == it || path.endsWith(it) }?.let { endpointPath ->
            path = path.removeSuffix(endpointPath).ifEmpty { "/" }
        }
        return URI(uri.scheme, null, uri.host, uri.port, path, null, null).toString().removeSuffix("/")
    }

    fun resolveOrganizationOptionsUrl(rawValue: String): String {
        val baseUrl = normalizeMetricsApiBaseUrl(rawValue)
        require(baseUrl.isNotEmpty()) { "请先填写上报服务器地址" }
        val uri = URI(baseUrl)
        val basePath = uri.path.orEmpty().replace(Regex("/+${'$'}"), "")
        return URI(uri.scheme, null, uri.host, uri.port, "$basePath$ORGANIZATION_OPTIONS_PATH", null, null).toString()
    }

    fun normalizeOrganizationOptions(json: String): OrganizationOptions {
        val root = try {
            JsonParser.parseString(json).asJsonObject
        } catch (_: Exception) {
            throw IllegalArgumentException("组织架构响应格式无效")
        }
        val rawDepartments = root.getAsJsonArray("departments")
            ?: throw IllegalArgumentException("组织架构响应缺少部门列表")
        val departments = rawDepartments.mapNotNull { departmentElement ->
            val department = departmentElement.asObjectOrNull() ?: return@mapNotNull null
            val name = department.string("name") ?: return@mapNotNull null
            val rawOffices = department.getAsJsonArray("offices") ?: return@mapNotNull null
            val offices = rawOffices.mapNotNull { officeElement ->
                val office = officeElement.asObjectOrNull() ?: return@mapNotNull null
                val officeName = office.string("name") ?: return@mapNotNull null
                val teams = LinkedHashSet<String>()
                office.getAsJsonArray("teams")?.forEach { teamElement ->
                    teamElement.asStringOrNull()?.trim()?.takeIf { it.isNotEmpty() }?.let(teams::add)
                }
                OrganizationOffice(officeName, teams.toList())
            }
            OrganizationDepartment(name, offices)
        }.filter { it.offices.isNotEmpty() }
        if (departments.isEmpty()) throw IllegalArgumentException("上报服务器未返回可用组织架构")
        return OrganizationOptions(
            version = root.get("version")?.takeIf { it.isJsonPrimitive }?.asIntOrNull(),
            departments = departments,
        )
    }

    fun validate(settings: ReportingSettings, organizationOptions: OrganizationOptions?): String? {
        val normalized = try {
            normalizeSettings(settings)
        } catch (error: IllegalArgumentException) {
            return error.message ?: "上报服务器地址无效"
        }
        if (normalized.metricsApiBaseUrl.isBlank()) return "请填写上报服务器地址"
        if (normalized.profile.departmentName.isBlank()) return "请选择部门"
        if (normalized.profile.officeName.isBlank()) return "请选择处"
        if (normalized.profile.userName.isBlank()) return "请填写姓名"
        if (!isValidEmail(normalized.profile.userEmail)) return "请填写有效的公司邮箱"
        if (organizationOptions == null) return null
        val department = organizationOptions.departments.find { it.name == normalized.profile.departmentName }
            ?: return "当前部门已失效，请重新选择"
        val office = department.offices.find { it.name == normalized.profile.officeName }
            ?: return "当前处已失效，请重新选择"
        if (office.teams.isNotEmpty() && normalized.profile.teamName.isBlank()) return "请选择组"
        if (office.teams.isNotEmpty() && normalized.profile.teamName.isNotBlank() && normalized.profile.teamName !in office.teams) {
            return "当前组已失效，请重新选择"
        }
        return null
    }

    fun offices(settings: ReportingSettings, options: OrganizationOptions?): List<OrganizationOffice> =
        options?.departments?.find { it.name == settings.profile.departmentName }?.offices.orEmpty()

    fun teams(settings: ReportingSettings, options: OrganizationOptions?): List<String> =
        offices(settings, options).find { it.name == settings.profile.officeName }?.teams.orEmpty()

    fun fromCliJson(json: String): ReportingSettings {
        val root = JsonParser.parseString(json).asJsonObject
        val profile = root.get("reporting_profile")?.asObjectOrNull() ?: JsonObject()
        return ReportingSettings(
            metricsApiBaseUrl = root.string("metrics_api_base_url").orEmpty(),
            profile = ReportingProfile(
                departmentName = profile.string("department_name").orEmpty(),
                officeName = profile.string("office_name").orEmpty(),
                teamName = profile.string("team_name").orEmpty(),
                userName = profile.string("user_name").orEmpty(),
                userEmail = profile.string("user_email").orEmpty(),
            ),
        )
    }

    fun toCliJson(settings: ReportingSettings): String {
        val normalized = normalizeSettings(settings)
        val profile = JsonObject().apply {
            addProperty("department_name", normalized.profile.departmentName)
            addProperty("office_name", normalized.profile.officeName)
            if (normalized.profile.teamName.isNotEmpty()) addProperty("team_name", normalized.profile.teamName)
            addProperty("user_name", normalized.profile.userName)
            addProperty("user_email", normalized.profile.userEmail)
        }
        return JsonObject().apply {
            addProperty("metrics_api_base_url", normalized.metricsApiBaseUrl)
            add("reporting_profile", profile)
        }.let(gson::toJson)
    }

    private fun normalizeForMerge(settings: ReportingSettings, preserveInvalidAddress: Boolean): ReportingSettings {
        val address = try {
            normalizeMetricsApiBaseUrl(settings.metricsApiBaseUrl)
        } catch (_: IllegalArgumentException) {
            if (preserveInvalidAddress) settings.metricsApiBaseUrl.trim() else ""
        }
        return ReportingSettings(address, ReportingProfile(
            departmentName = settings.profile.departmentName.trim(),
            officeName = settings.profile.officeName.trim(),
            teamName = settings.profile.teamName.trim(),
            userName = settings.profile.userName.trim(),
            userEmail = settings.profile.userEmail.trim().lowercase(),
        ))
    }

    private fun JsonElement.asObjectOrNull(): JsonObject? = if (isJsonObject) asJsonObject else null
    private fun JsonElement.asStringOrNull(): String? = if (isJsonPrimitive) runCatching { asString }.getOrNull() else null
    private fun JsonElement.asIntOrNull(): Int? = runCatching { asInt }.getOrNull()
    private fun JsonObject.string(name: String): String? = get(name)?.asStringOrNull()?.trim()?.takeIf { it.isNotEmpty() }
    private fun isValidEmail(value: String): Boolean = value.indexOf('@') in 1 until value.length - 3 && value.substringAfter('@').contains('.')
}
