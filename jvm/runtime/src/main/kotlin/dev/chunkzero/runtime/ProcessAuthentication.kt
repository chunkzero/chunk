package dev.chunkzero.runtime

import io.grpc.Metadata
import io.grpc.ServerCall
import io.grpc.ServerCallHandler
import io.grpc.ServerInterceptor
import io.grpc.Status

internal class ProcessAuthentication(
    private val token: String,
) : ServerInterceptor {
    init {
        require(token.length >= 32) { "Process token must contain at least 32 characters" }
    }

    override fun <ReqT : Any, RespT : Any> interceptCall(
        call: ServerCall<ReqT, RespT>,
        headers: Metadata,
        next: ServerCallHandler<ReqT, RespT>,
    ): ServerCall.Listener<ReqT> {
        if (headers.get(Metadata.Key.of("authorization", Metadata.ASCII_STRING_MARSHALLER)) != "Bearer $token") {
            call.close(Status.UNAUTHENTICATED, Metadata())
            return object : ServerCall.Listener<ReqT>() {}
        }
        return next.startCall(call, headers)
    }
}
