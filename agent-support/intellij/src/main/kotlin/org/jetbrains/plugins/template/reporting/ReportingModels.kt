package org.jetbrains.plugins.template.reporting

data class ReportingProfile(
    val departmentName: String = "",
    val officeName: String = "",
    val teamName: String = "",
    val userName: String = "",
    val userEmail: String = "",
)

data class ReportingSettings(
    val metricsApiBaseUrl: String = "",
    val profile: ReportingProfile = ReportingProfile(),
)

data class OrganizationOffice(
    val name: String,
    val teams: List<String>,
)

data class OrganizationDepartment(
    val name: String,
    val offices: List<OrganizationOffice>,
)

data class OrganizationOptions(
    val version: Int? = null,
    val departments: List<OrganizationDepartment>,
)

enum class OrganizationSource { SERVER, CACHE }

data class OrganizationLoadResult(
    val endpoint: String? = null,
    val options: OrganizationOptions? = null,
    val source: OrganizationSource? = null,
    val fetchedAt: Long? = null,
    val error: String? = null,
)

data class InitialReportingState(
    val settings: ReportingSettings,
    val importedFields: List<String>,
    val organization: OrganizationLoadResult,
    val cliError: String? = null,
)
