package org.jetbrains.plugins.template.reporting

import com.intellij.util.messages.Topic

fun interface ReportingProfileListener {
    fun profileSaved(settings: ReportingSettings)
}

object ReportingProfileTopics {
    val PROFILE_SAVED: Topic<ReportingProfileListener> = Topic.create("Git AI reporting profile saved", ReportingProfileListener::class.java)
}
