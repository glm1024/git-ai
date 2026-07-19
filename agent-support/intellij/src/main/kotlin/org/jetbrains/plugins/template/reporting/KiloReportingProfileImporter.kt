package org.jetbrains.plugins.template.reporting

import com.google.gson.JsonObject
import com.google.gson.JsonParser
import com.intellij.ide.util.PropertiesComponent
import com.intellij.openapi.application.PathManager
import org.w3c.dom.Element
import java.io.ByteArrayOutputStream
import java.nio.file.Files
import java.nio.file.Path
import javax.xml.XMLConstants
import javax.xml.parsers.DocumentBuilderFactory

/** Reads Kilo values only to fill blank Git AI fields; it never writes Kilo state. */
class KiloReportingProfileImporter(
    private val properties: PropertiesComponent = PropertiesComponent.getInstance(),
    private val optionsPath: Path = Path.of(PathManager.getOptionsPath()),
) {
    fun read(): ReportingSettings {
        val current = KILO_FIELDS.associateWith { key ->
            properties.getValue("user.kilo-code.$key").orEmpty().trim()
        }
        val legacy = readLegacyValues()
        fun value(key: String): String = current[key].orEmpty().ifEmpty { legacy[key].orEmpty() }
        return ReportingSettings(
            metricsApiBaseUrl = value("aiCodeStatsWebhookUrl"),
            profile = ReportingProfile(
                departmentName = value("aiCodeStatsDepartmentName"),
                officeName = value("aiCodeStatsOfficeName"),
                teamName = value("aiCodeStatsTeamName"),
                userName = value("aiCodeStatsUserName"),
                userEmail = value("aiCodeStatsUserEmail"),
            ),
        )
    }

    private fun readLegacyValues(): Map<String, String> {
        val storage = optionsPath.resolve("kilocode-extension-storage.xml")
        if (!Files.isRegularFile(storage)) return emptyMap()
        return runCatching {
            parseLegacyStorageXml(readAtMost(storage, MAX_LEGACY_STORAGE_BYTES))
        }.getOrDefault(emptyMap())
    }

    companion object {
        private const val MAX_LEGACY_STORAGE_BYTES = 2 * 1024 * 1024
        private const val KILO_COMPONENT = "ai.kilocode.jetbrains.service.ExtensionStorageService"
        private const val KILO_STORAGE_KEY = "Kilo Code.kilo-code"
        private val KILO_FIELDS = listOf(
            "aiCodeStatsWebhookUrl",
            "aiCodeStatsDepartmentName",
            "aiCodeStatsOfficeName",
            "aiCodeStatsTeamName",
            "aiCodeStatsUserName",
            "aiCodeStatsUserEmail",
        )

        internal fun parseLegacyStorageXml(xml: ByteArray): Map<String, String> {
            val document = secureDocumentBuilderFactory().newDocumentBuilder().parse(xml.inputStream())
            val component = (0 until document.getElementsByTagName("component").length)
                .map { document.getElementsByTagName("component").item(it) as? Element }
                .firstOrNull { it?.getAttribute("name") == KILO_COMPONENT } ?: return emptyMap()
            val storageMap = (0 until component.getElementsByTagName("map").length)
                .map { component.getElementsByTagName("map").item(it) as? Element }
                .firstOrNull { it?.getAttribute("name") == "storageMap" } ?: return emptyMap()
            val storedJson = (0 until storageMap.getElementsByTagName("entry").length)
                .map { storageMap.getElementsByTagName("entry").item(it) as? Element }
                .firstOrNull { it?.getAttribute("key") == KILO_STORAGE_KEY }
                ?.getAttribute("value")
                ?.takeIf { it.isNotBlank() } ?: return emptyMap()
            return parseStoredSettings(storedJson)
        }

        private fun parseStoredSettings(json: String): Map<String, String> {
            val root = runCatching { JsonParser.parseString(json).asJsonObject }.getOrNull() ?: return emptyMap()
            return KILO_FIELDS.mapNotNull { key ->
                root.string(key)?.let { key to it }
            }.toMap()
        }

        private fun JsonObject.string(name: String): String? = get(name)
            ?.takeIf { it.isJsonPrimitive }
            ?.let { runCatching { it.asString.trim() }.getOrNull() }
            ?.takeIf { it.isNotEmpty() }

        private fun secureDocumentBuilderFactory(): DocumentBuilderFactory = DocumentBuilderFactory.newInstance().apply {
            isNamespaceAware = false
            isXIncludeAware = false
            isExpandEntityReferences = false
            setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)
            setFeature("http://xml.org/sax/features/external-general-entities", false)
            setFeature("http://xml.org/sax/features/external-parameter-entities", false)
            setFeature("http://apache.org/xml/features/nonvalidating/load-external-dtd", false)
            setAttribute(XMLConstants.ACCESS_EXTERNAL_DTD, "")
            setAttribute(XMLConstants.ACCESS_EXTERNAL_SCHEMA, "")
        }

        private fun readAtMost(path: Path, maxBytes: Int): ByteArray = Files.newInputStream(path).use { input ->
            val output = ByteArrayOutputStream()
            val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                if (output.size() + count > maxBytes) throw IllegalArgumentException("Kilo storage is too large")
                output.write(buffer, 0, count)
            }
            output.toByteArray()
        }
    }
}
