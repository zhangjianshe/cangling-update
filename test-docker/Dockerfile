FROM nginx:1.27-alpine

ARG VERSION=0.0.0
LABEL org.opencontainers.image.title="cangling-test"
LABEL org.opencontainers.image.version="${VERSION}"

COPY nginx.conf /etc/nginx/nginx.conf
COPY index.html /tmp/index.html
RUN sed "s/__VERSION__/${VERSION}/g" /tmp/index.html > /usr/share/nginx/html/index.html \
    && printf '%s\n' "$VERSION" > /usr/share/nginx/html/version.txt \
    && rm /tmp/index.html

EXPOSE 80
