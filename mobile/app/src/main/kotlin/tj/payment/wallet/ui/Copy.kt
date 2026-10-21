package tj.payment.wallet.ui

/**
 * UI copy produced outside composables (ViewModel state, error mapping) comes
 * from string resources — localized by the device language (values/ English,
 * values-ru/ Russian; Tajik pending) — through this one resolver. Composables
 * use `stringResource` directly.
 *
 * Pure Kotlin on purpose: [PaymentApp] installs the Android `Resources` lookup
 * at startup; JVM unit tests keep the default, which renders `#<id>(args)` so
 * assertions can check structure without Android resources.
 */
object Copy {
    @Volatile
    private var resolve: (Int, Array<out Any>) -> String = { id, args ->
        "#$id" + if (args.isEmpty()) "" else args.joinToString(prefix = "(", postfix = ")")
    }

    /** [resolver] gets a string-resource id and its format args (possibly none). */
    fun install(resolver: (Int, Array<out Any>) -> String) {
        resolve = resolver
    }

    fun text(id: Int, vararg args: Any): String = resolve(id, args)
}
