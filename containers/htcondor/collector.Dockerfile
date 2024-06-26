FROM htcondor/submit:23.5.2-el8

RUN yum install -y python39 python39-pip && \
    yum clean all && \
    rm -rf /var/cache/yum/*

RUN python3.9 -m pip install --upgrade pip pyyaml

COPY ./condor_passwords /etc/condor/passwords-orig.d/
COPY ./condor_passwords/POOL /home/submituser/POOL
RUN chown submituser:submituser /home/submituser/POOL
RUN chmod 600 /home/submituser/POOL

RUN install -d -m 0700 -o submituser -g submituser /home/submituser/.condor
RUN echo "SEC_PASSWORD_FILE=/home/submituser/POOL" >> /home/submituser/.condor/user_config
