package org.jetbrains.plugins.template.reporting

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ReportingSupportTest {
    @Test
    fun normalizesUploadEndpointToServerBaseUrl() {
        assertEquals(
            "https://stats.example.com/prod-api",
            ReportingSupport.normalizeMetricsApiBaseUrl(" https://stats.example.com/prod-api/worker/metrics/upload "),
        )
        assertEquals(
            "https://stats.example.com",
            ReportingSupport.normalizeMetricsApiBaseUrl("https://stats.example.com/api/v1/ingest/ai-code-stats"),
        )
    }

    @Test
    fun mergeKeepsMalformedGitAiAddressEditableAndImportsOnlyBlankFields() {
        val saved = ReportingSettings(
            metricsApiBaseUrl = "not a url",
            profile = ReportingProfile(userName = "Git AI User"),
        )
        val kilo = ReportingSettings(
            metricsApiBaseUrl = "https://stats.example.com/upload",
            profile = ReportingProfile(departmentName = "研发部", userName = "Kilo User", userEmail = "USER@EXAMPLE.COM"),
        )

        assertEquals(
            ReportingSettings(
                metricsApiBaseUrl = "not a url",
                profile = ReportingProfile(departmentName = "研发部", userName = "Git AI User", userEmail = "user@example.com"),
            ),
            ReportingSupport.mergeSettings(saved, kilo),
        )
    }

    @Test
    fun validatesOrganizationMembershipAndOptionalTeam() {
        val options = ReportingSupport.normalizeOrganizationOptions(
            """{"version":1,"departments":[{"name":"研发部","offices":[{"name":"一处","teams":["一组","一组"]},{"name":"二处","teams":[]}]}]}""",
        )
        val settings = ReportingSettings(
            "https://stats.example.com",
            ReportingProfile("研发部", "一处", "", "张三", "zhang@example.com"),
        )
        assertEquals("请选择组", ReportingSupport.validate(settings, options))
        assertNull(ReportingSupport.validate(settings.copy(profile = settings.profile.copy(teamName = "一组")), options))
        assertNull(ReportingSupport.validate(settings.copy(profile = settings.profile.copy(officeName = "二处", teamName = "旧组")), options))
        assertTrue(options.departments.single().offices.first().teams == listOf("一组"))
    }

    @Test
    fun readsLegacyKiloStorageWithoutUsingExternalEntities() {
        val xml = """
            <application>
              <component name="ai.kilocode.jetbrains.service.ExtensionStorageService">
                <map name="storageMap">
                  <entry key="Kilo Code.kilo-code" value="{&quot;aiCodeStatsWebhookUrl&quot;:&quot;http://localhost:8082&quot;,&quot;aiCodeStatsUserEmail&quot;:&quot;USER@EXAMPLE.COM&quot;}" />
                </map>
              </component>
            </application>
        """.trimIndent().toByteArray()

        assertEquals(
            mapOf(
                "aiCodeStatsWebhookUrl" to "http://localhost:8082",
                "aiCodeStatsUserEmail" to "USER@EXAMPLE.COM",
            ),
            KiloReportingProfileImporter.parseLegacyStorageXml(xml),
        )
    }
}
