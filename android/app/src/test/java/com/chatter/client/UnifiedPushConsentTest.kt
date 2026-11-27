package com.chatter.client

import org.junit.Assert.assertEquals
import org.junit.Test

class UnifiedPushConsentTest {
    @Test
    fun installedDistributorIsOfferedNotPicked() {
        assertEquals(listOf("io.heckel.ntfy"), pickableDistributors(null, listOf("io.heckel.ntfy"), emptySet()))
    }

    @Test
    fun nothingIsOfferedOnceTheUserChose() {
        assertEquals(emptyList<String>(), pickableDistributors("io.heckel.ntfy", listOf("io.heckel.ntfy", "org.other"), emptySet()))
    }

    @Test
    fun aDistributorSavedWithoutConsentIsNotChosen() {
        // Auto-enrolled by an older build: no consent recorded.
        assertEquals(null, consentedDistributor("io.heckel.ntfy", null))
        // Consent was for another package than the one now saved.
        assertEquals(null, consentedDistributor("org.other", "io.heckel.ntfy"))
        assertEquals("io.heckel.ntfy", consentedDistributor("io.heckel.ntfy", "io.heckel.ntfy"))
        assertEquals(null, consentedDistributor(null, "io.heckel.ntfy"))
        // An unconsented saved one is unregistered, then offered like a fresh install.
        assertEquals(
            listOf("io.heckel.ntfy"),
            pickableDistributors(consentedDistributor("io.heckel.ntfy", null), listOf("io.heckel.ntfy"), emptySet()),
        )
    }

    @Test
    fun declinedDistributorsAreNotOfferedAgainButNewOnesAre() {
        val declined = setOf("io.heckel.ntfy")
        assertEquals(emptyList<String>(), pickableDistributors(null, listOf("io.heckel.ntfy"), declined))
        assertEquals(listOf("org.other"), pickableDistributors(null, listOf("io.heckel.ntfy", "org.other"), declined))
    }
}
