# Tink (via androidx.security-crypto) references errorprone annotations that are
# compile-only and absent at runtime — safe to ignore.
-dontwarn com.google.errorprone.annotations.**
-dontwarn javax.annotation.**

# kotlinx.serialization keeps generated serializers; keep them for our DTOs.
-keepclassmembers class tj.payment.** {
    *** Companion;
}
-keepclasseswithmembers class tj.payment.** {
    kotlinx.serialization.KSerializer serializer(...);
}
-keep,includedescriptorclasses class tj.payment.**$$serializer { *; }
